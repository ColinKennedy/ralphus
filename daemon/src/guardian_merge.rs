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

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};

use crate::cancel::{CancelToken, Cancellations};
use crate::guardian::{CheckInput, CheckInputType, GuardianCheck, GuardianStatus, MergeStatus};
use crate::runner::{Runner, RunnerSpec};
use crate::scheduler::Semaphore;
use crate::server::Reply;
use crate::store::Store;
use crate::vcs::{GitOps, GitVcs};
use crate::workspace::Workspace;

/// The `RunnerSpec.task` value used for every conflict-resolver invocation
/// (RAL-102). `server.rs`'s guardian-branch terminal/pane endpoints must pass
/// this exact string into `crate::tmux::session_name` to recompute the tmux
/// session name the resolver actually runs under — kept as a shared constant
/// rather than a literal duplicated in both files so the two can never drift.
pub(crate) const RESOLVER_TASK: &str = "resolve";

/// The `RunnerSpec.task` value used for the dedicated final-proof
/// invocation that follows a branch's fix pass (RAL-149) — a distinct LLM
/// call from [`RESOLVER_TASK`] so the indicator on the board reflects a real,
/// separate step rather than something implicitly bundled into the fix call.
pub(crate) const RESOLVER_PROOF_TASK: &str = "resolve-proof";

/// The `RunnerSpec.task`/`cell_id` values used for every manual-checks
/// generation invocation (RAL-88 follow-up). Mirrors [`RESOLVER_TASK`]'s
/// rationale: `server.rs`'s manual-checks terminal/pane endpoints must pass
/// these exact strings into `crate::tmux::session_name` to recompute the tmux
/// session name `generate_manual_commands` actually runs under.
pub(crate) const MANUAL_COMMANDS_TASK: &str = "manual_commands";
pub(crate) const MANUAL_COMMANDS_SESSION: &str = "manual-reviewer";
const AUTO_BUILD_TASK: &str = "auto_build";
const AUTO_BUILD_SESSION: &str = "auto-build";
/// Task name for "set it for me" input resolution (RAL-164) -- see
/// [`resolve_check_input`].
pub(crate) const RESOLVE_INPUT_TASK: &str = "resolve_input";

/// The `RunnerSpec.task` value used for every reviewer-feedback-actioning
/// invocation ([`run_feedback`], RAL-298). Mirrors [`RESOLVER_TASK`]'s
/// rationale -- `server.rs`'s `resolver_task_and_cell_id` must pass this
/// exact string into `crate::tmux::session_name` to recompute the tmux
/// session name a feedback pass actually runs under, both live and after
/// the fact.
pub(crate) const FEEDBACK_TASK: &str = "feedback";

/// The `RunnerSpec.cell_id` for one branch's feedback-actioning session --
/// branch-scoped (RAL-298) the same way [`RESOLVER_TASK`]'s `resolver-
/// {branch_id}` is, so two branches under the same guardian actioning
/// feedback concurrently don't collide on the same tmux session (the
/// squad_id `guardian-{id}` alone is shared by every branch in the
/// guardian).
pub(crate) fn feedback_cell_id(branch_id: &str) -> String {
    format!("reviewer-{branch_id}")
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum StartMergeOutcome {
    Merging,
    Deferred,
    AlreadyInProgress,
    /// RAL-300: every linked PR had already merged, so this trigger approved
    /// the review outright instead of starting a rebuild.
    AlreadyMerged,
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

/// Marker the conflict-resolver agent outputs after `git add -A` to signal the
/// orchestrator that the index is ready for `git rebase --continue`.
const STAGE_DONE_MARKER: &str = "RALPHUS_STAGE: DONE";

/// Run `git` with `args` in `root`, returning stdout on success or a message.
///
/// Thin wrapper over [`crate::vcs::GitOps::run`] — the actual `git` subprocess
/// spawn lives in `vcs.rs`, not here (RAL-213). Kept as a free function
/// because it is called throughout this module (directly, and via
/// [`Workspace::git`]) far too pervasively to thread a `GitVcs` value through
/// every call site.
pub(crate) fn git(root: &Path, args: &[&str]) -> std::result::Result<String, String> {
    GitVcs.run(root, args)
}

/// Push `wt`'s current `local_branch` after a feedback commit (RAL-<new>).
///
/// Deliberately not PR-system-aware: this doesn't know or care whether
/// `local_branch` is itself an open PR's branch (in which case the PR just
/// shows the change immediately) or the basis a separate PR branch was built
/// from (whose own rebase is a different, already-existing concern).
///
/// `fork_remote` (RAL-338): when `Some`, always pushes there, ignoring
/// `@{upstream}`/`remote.pushDefault` entirely -- the project has a
/// registered fork, and every review branch lives there regardless of what a
/// prior `git push -u` may have set `@{upstream}` to (a stray push can
/// rewrite it and cause the fork remote to be mistaken for the parent's; see
/// this ticket's Risks section). When `None` (no registered fork), behavior
/// is unchanged: the branch's already-configured upstream (`@{u}`) when one
/// exists, else the git default remote (`remote.pushDefault`, else
/// `"origin"`), pushing to a same-named remote branch and setting upstream
/// tracking on that first push so later feedback pushes on this branch
/// naturally follow `@{u}` from then on.
///
/// `force`: pass `false` when the local commit was `--amend`ed onto history
/// the remote already has an older version of (the previous push already
/// forced that commit into place, so a plain push replacing it with the
/// amended version is still exactly the kind of rewrite the remote already
/// expects); pass `true` for a plain new commit stacked on top, which is
/// what needs `--force` if the remote branch was itself previously
/// force-pushed to a divergent tip. Guarded by [`guard_against_clobber`]
/// before any force-push, so a reviewer's own direct push to the same remote
/// branch is never silently discarded.
pub(crate) fn push_feedback_branch(
    wt: &Workspace,
    local_branch: &str,
    force: bool,
    fork_remote: Option<&str>,
) -> std::result::Result<String, String> {
    let (remote, remote_branch) = if let Some(fork_remote) = fork_remote {
        (fork_remote.to_string(), local_branch.to_string())
    } else {
        match wt.git(&["rev-parse", "--abbrev-ref", "--symbolic-full-name", "@{u}"]) {
            Ok(upstream) => {
                let upstream = upstream.trim();
                match upstream.split_once('/') {
                    Some((remote, branch)) => (remote.to_string(), branch.to_string()),
                    None => (upstream.to_string(), local_branch.to_string()),
                }
            }
            Err(_) => {
                let remote = wt
                    .git(&["config", "remote.pushDefault"])
                    .ok()
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty())
                    .unwrap_or_else(|| "origin".to_string());
                (remote, local_branch.to_string())
            }
        }
    };

    if force {
        guard_against_clobber(wt, &remote, &remote_branch, local_branch)?;
    }

    let refspec = format!("{local_branch}:refs/heads/{remote_branch}");
    let mut args = vec!["push"];
    if force {
        args.push("--force");
    }
    args.push("--set-upstream");
    args.push(&remote);
    args.push(&refspec);
    wt.git(&args)?;

    wt.git(&["rev-parse", "HEAD"]).map(|s| s.trim().to_string())
}

/// Workspace-routed counterpart of `pr::guard_against_clobber` — refuse to
/// force-push over commits the remote `remote_branch` has that
/// `local_branch` does not (e.g. a reviewer pushed a fix directly to the
/// branch). A remote branch that doesn't exist yet, or one whose tip is
/// already an ancestor of `local_branch` (so the force-push is a strict
/// superset), is safe and returns `Ok(())`. Only needed ahead of a
/// force-push — a plain push is already safely rejected by git itself on
/// divergence.
///
/// A restack rewrites every SHA it replays, so a remote holding nothing but
/// the pre-restack spelling of `local_branch`'s own commits is un-ancestored
/// yet carries no work to lose; the patch-id fallback recognizes that case.
fn guard_against_clobber(
    wt: &Workspace,
    remote: &str,
    remote_branch: &str,
    local_branch: &str,
) -> std::result::Result<(), String> {
    if wt.git(&["fetch", remote, remote_branch]).is_err() {
        return Ok(());
    }
    let Ok(remote_sha) = wt.git(&["rev-parse", "FETCH_HEAD"]) else {
        return Ok(());
    };
    let remote_sha = remote_sha.trim();
    if wt
        .git(&["merge-base", "--is-ancestor", remote_sha, local_branch])
        .is_ok()
    {
        return Ok(());
    }
    // `+`-prefixed lines are remote commits with no patch-equivalent in
    // `local_branch`; none means the remote is a replayed ancestor in all but
    // SHA. See `pr::guard_against_clobber` for the same check.
    if let Ok(cherry) = wt.git(&["cherry", local_branch, remote_sha]) {
        if !cherry.lines().any(|l| l.starts_with('+')) {
            return Ok(());
        }
    }
    Err(format!(
        "remote branch '{remote_branch}' has commits not present in the review \
         worktree (a reviewer likely pushed directly to it) -- pull those commits \
         into the worktree first instead of overwriting them"
    ))
}

/// The `.git/worktrees/<name>` admin-entry name for `wt`, resolved rather than
/// assumed from its directory's basename (RAL-211).
///
/// `.git/worktrees/` is a FLAT namespace shared by every worktree in the
/// repository, but the short directory names under `.git/.ralphus/` make it
/// possible for two worktrees that are otherwise unrelated -- e.g. two
/// different guardians both naming a branch `wt-RAL-121` in their own
/// `g/g<n>/` directory -- to share a basename. Blindly using the basename as
/// the admin name would then
/// make [`relink_worktree`] target the SAME `.git/worktrees/wt-RAL-121/`
/// entry for both, silently cross-wiring them (last write wins).
///
/// Prefers the name `wt`'s own `.git` file already records: whenever a
/// worktree has ever been linked (via `git worktree add` or a prior relink),
/// that file's `gitdir: .../worktrees/<name>` line names the admin entry git
/// itself already gave it -- correct and unique by construction, even if the
/// entry directory itself was since deleted (the exact case `relink_worktree`
/// exists to repair; the pointer's *content* survives even when its *target*
/// doesn't). Only when no such pointer survives (the worktree has never been
/// linked, or its `.git` file is unreadable) is a name invented, and then
/// it's qualified with the worktree's own parent directory -- the guardian's
/// `g<n>` folder (unique per guardian) under `.git/.ralphus/g/` -- rather
/// than the bare basename, so
/// two guardians can never collide with each other here even though nothing
/// yet exists to read a real name from.
fn admin_entry_name(wt: &Workspace) -> String {
    if let Some(existing) = wt.read_file(".git") {
        if let Some(name) = existing
            .trim()
            .strip_prefix("gitdir:")
            .map(str::trim)
            .and_then(|p| Path::new(p).file_name())
        {
            return name.to_string_lossy().into_owned();
        }
    }
    let basename = wt
        .root()
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    match wt.root().parent().and_then(Path::file_name) {
        Some(parent) => format!("{}-{basename}", parent.to_string_lossy()),
        None => basename,
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
    let name = admin_entry_name(wt);
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
        let _ = root.git(&["update-ref", "--delete", name]);
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
            let _ = git(root, &["update-ref", "--delete", name]);
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
            // ralphus[ignore-rlog-pair]: this low-level helper has no Store; its Store-owning caller records the structured workflow outcome
            crate::rlog!(
                WARNING,
                "ralphus [guardian] worktree recovery: feature branch '{branch}' is absent; \
                 creating '{rev}' from feature-worktree HEAD {sha} — prior review commits lost"
            );
            let _ = root.git(&["branch", "--delete", "--force", rev]);
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
    worktree_add_or_reset_with_faults(root, rev, wt, branch, &mut NoRecoveryFaults)
}

trait RecoveryFaults {
    fn checkout_error(&mut self) -> Option<String> {
        None
    }

    fn remove_error(&mut self) -> Option<String> {
        None
    }
}

struct NoRecoveryFaults;

impl RecoveryFaults for NoRecoveryFaults {}

/// Test seam for the two transient failures involved in final worktree
/// recovery. All non-injected VCS operations still use [`Workspace::git`].
fn worktree_add_or_reset_with_faults<F>(
    root: &Workspace,
    rev: &str,
    wt: &Workspace,
    branch: &str,
    faults: &mut F,
) -> std::result::Result<(), String>
where
    F: RecoveryFaults,
{
    let wt_str = wt.root().to_string_lossy().to_string();

    if !wt.root().exists() {
        // [State 1] Directory missing — fast path.
        // [State 7] Unlock first: a locked tracking entry survives both the
        // `remove` below (single `-f` does not override a lock) and `prune`
        // (which skips locked entries by design), leaving the branch "already
        // used by worktree" at this exact path forever even though nothing is
        // actually checked out there. No-op when not locked.
        let _ = root.git(&["worktree", "unlock", &wt_str]);
        // Remove any stale tracking entry for this path (quick no-op when not
        // registered). This handles [State 4] when git's remove succeeds on a
        // ghost entry; if it does not, the lazy prune below is the fallback.
        let _ = root.git(&["worktree", "remove", "--force", &wt_str]);

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
            // ralphus[ignore-rlog-pair]: this low-level helper has no Store; its Store-owning caller records the structured workflow outcome
            crate::rlog!(
                WARNING,
                "ralphus [guardian] worktree recovery: discarding uncommitted changes in {wt_str} \
                 before resetting to '{branch}'"
            );
        }
        let checkout = faults
            .checkout_error()
            .map_or_else(|| wt.git(&["checkout", "--force", "-B", rev, branch]), Err);
        if checkout.is_ok() {
            return Ok(());
        }
    }

    // [State 7] Unlock if locked. No-op when not locked.
    let _ = root.git(&["worktree", "unlock", &wt_str]);

    // Remove any existing tracking entry for this path.
    // One `-f` handles dirty/untracked files; a second `-f` handles locked
    // worktrees (belt-and-suspenders after the explicit unlock above).
    let remove = faults.remove_error().map_or_else(
        || root.git(&["worktree", "remove", "--force", &wt_str]),
        Err,
    );
    log_worktree_remove_attempt(&wt_str, 1, &remove);
    let remove = faults.remove_error().map_or_else(
        || root.git(&["worktree", "remove", "--force", "--force", &wt_str]),
        Err,
    );
    log_worktree_remove_attempt(&wt_str, 2, &remove);

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
        // ralphus[ignore-rlog-pair]: this low-level helper has no Store; its Store-owning caller records the structured workflow outcome
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
        // ralphus[ignore-rlog-pair]: this low-level helper has no Store; its Store-owning caller records the structured workflow outcome
        crate::rlog!(
            WARNING,
            "ralphus [guardian] worktree recovery: discarding uncommitted changes in {wt_str} \
             before resetting to '{branch}'"
        );
    }

    // [State 3] Review branch missing: `-B` creates it.
    // [State 5] Branch mismatch: `-B` resets to the correct starting point.
    // [State 6] Detached HEAD: `checkout` reattaches to a named branch.
    // `--force` discards local modifications.
    let checkout = faults
        .checkout_error()
        .map_or_else(|| wt.git(&["checkout", "--force", "-B", rev, branch]), Err);
    match checkout {
        Ok(_) => Ok(()),
        Err(checkout_err) => {
            // Feature branch may be absent; wipe and regenerate.
            wt.remove_path(".", true);
            // The preceding worktree removals may have failed transiently on
            // Windows while the directory still existed. Now that it is gone,
            // prune the leftover registration before trying to add it back.
            let _ = root.git(&["worktree", "prune"]);
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

fn log_worktree_remove_attempt(
    wt: &str,
    force_count: u8,
    result: &std::result::Result<String, String>,
) {
    match result {
        // ralphus[ignore-rlog-pair]: this low-level helper has no Store; its Store-owning caller records the structured workflow outcome
        Ok(_) => crate::rlog!(
            DEBUG,
            "ralphus [guardian] worktree recovery: remove path={wt:?} forces={force_count} outcome=ok"
        ),
        // ralphus[ignore-rlog-pair]: this low-level helper has no Store; its Store-owning caller records the structured workflow outcome
        Err(error) => crate::rlog!(
            DEBUG,
            "ralphus [guardian] worktree recovery: remove path={wt:?} forces={force_count} outcome=error error={error:?}"
        ),
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

/// RAL-330: per-file `(added, deleted)` line counts from `git diff --numstat`,
/// run with `extra_args` appended (e.g. `[old, new]` for a commit-to-commit
/// diff, or a single ref to diff against the current working tree/index).
/// Binary files report `-`/`-` for both counts and are silently dropped —
/// this check can't reason about binary content either way, so treating `-`
/// as `0` would make a binary file's real content loss invisible instead of
/// just unverifiable.
fn diff_numstat(
    wt: &Workspace,
    extra_args: &[&str],
) -> std::collections::BTreeMap<String, (u64, u64)> {
    // RAL-330: `--no-renames` is load-bearing, not cosmetic. With rename
    // detection on (git's default for `git diff`), a renamed path is
    // reported as `dir/{old => new}` -- a display string, not a usable
    // pathspec. `lost`'s entries flow straight into `git rerere forget` as
    // literal arguments (see the call site above); a `{old => new}` string
    // matches nothing there, so `forget` silently no-ops and the poisoned
    // rerere entry survives to replay on the very next retry.
    let mut args = vec!["diff", "--numstat", "--no-renames"];
    args.extend_from_slice(extra_args);
    wt.git(&args)
        .unwrap_or_default()
        .lines()
        .filter_map(|line| {
            let mut parts = line.splitn(3, '\t');
            let added = parts.next()?.parse::<u64>().ok()?;
            let deleted = parts.next()?.parse::<u64>().ok()?;
            let path = parts.next()?.to_string();
            Some((path, (added, deleted)))
        })
        .collect()
}

/// RAL-330: file paths changed by `git diff --name-only`, run with
/// `extra_args` appended the same way as [`diff_numstat`].
fn diff_name_set(wt: &Workspace, extra_args: &[&str]) -> HashSet<String> {
    // RAL-330: `--no-renames`, same reason as `diff_numstat` -- this set is
    // intersected against `diff_numstat`'s (also de-abbreviated) paths, so
    // the two must agree on path identity for a renamed file.
    let mut args = vec!["diff", "--name-only", "--no-renames"];
    args.extend_from_slice(extra_args);
    wt.git(&args)
        .unwrap_or_default()
        .lines()
        .map(str::to_string)
        .collect()
}

/// RAL-330: verify that a rebase step about to be finished — because no
/// conflict markers remain, whether `git rerere`'s fast path replayed a
/// cached resolution or a real agent fix was just staged — has not silently
/// dropped any of `REBASE_HEAD`'s own real content.
///
/// Anchored on git's own `REBASE_HEAD` (the commit currently being replayed)
/// rather than on either side's named base branch, so it is blind to the
/// task's base branch differing from the review's upstream, and to the task
/// and review having unrelated commit histories/counts (squash, a
/// review-worktree commit upstreamed from an unrelated PR branch, ...) — see
/// RAL-330's ticket for the concerns this addresses.
///
/// Only checks files the *new base* never touched at all between
/// `REBASE_HEAD`'s old and new parent: those had no legitimate reason to
/// change during this replay, so their diff against the new base must
/// exactly match their diff against the old parent. Files the new base did
/// touch are a genuine conflict zone where a correct merge is expected to
/// differ from either side alone, so this deliberately does not verify them
/// — exact-equality there would false-positive on every correctly-resolved
/// conflict, not just a lossy one.
///
/// Returns the paths that lost content, or `None` if nothing looks wrong —
/// including when there is no `REBASE_HEAD` to check against, which means
/// this isn't actually a paused per-commit rebase step.
fn detect_rebase_step_content_loss(wt: &Workspace) -> Option<Vec<String>> {
    let rebase_head = wt
        .git(&["rev-parse", "REBASE_HEAD"])
        .ok()?
        .trim()
        .to_string();
    if rebase_head.is_empty() {
        return None;
    }
    let old_parent = format!("{rebase_head}^");
    let new_base = wt.git(&["rev-parse", "HEAD"]).ok()?.trim().to_string();

    let original = diff_numstat(wt, &[old_parent.as_str(), rebase_head.as_str()]);
    if original.is_empty() {
        return None;
    }
    let base_delta = diff_name_set(wt, &[old_parent.as_str(), new_base.as_str()]);
    let replayed = diff_numstat(wt, &[new_base.as_str()]);

    let lost: Vec<String> = original
        .iter()
        .filter(|(path, _)| !base_delta.contains(*path))
        .filter(|(path, stat)| replayed.get(*path).copied() != Some(**stat))
        .map(|(path, _)| path.clone())
        .collect();

    if lost.is_empty() { None } else { Some(lost) }
}

/// RAL-330: run [`detect_rebase_step_content_loss`] against `REBASE_HEAD`
/// and, if it flags a loss, log it, poison the offending `rerere` cache
/// entries, abort the rebase, and return the failure -- shared by every call
/// site in [`resolve_conflicts_with_agent`]'s loop that is about to advance
/// past a step it just considers resolved (stage-done signal, the marker-count
/// fallback, and the loop's own top-of-iteration rerere fast path), since
/// `REBASE_HEAD` still names the commit-in-flight right up until
/// [`advance_rebase`] is called and stops being checkable afterward.
fn guard_against_rebase_step_content_loss(
    store: &Arc<Mutex<Store>>,
    id: &str,
    branch: &str,
    wt: &Workspace,
) -> std::result::Result<(), String> {
    let Some(lost) = detect_rebase_step_content_loss(wt) else {
        return Ok(());
    };
    let rebase_head = wt
        .git(&["rev-parse", "REBASE_HEAD"])
        .unwrap_or_default()
        .trim()
        .to_string();
    crate::rlog!(
        WARNING,
        "ralphus [guardian] review {id} content-preservation check failed branch={branch:?} \
         rebase_head={rebase_head:?} lost={lost:?}"
    );
    {
        let guard = store.lock().expect("poisoned");
        let _ = guard.cartographer_log(crate::cartographer::CartographerEntry {
            level: crate::logging::LogLevel::WARNING,
            source: "guardian",
            message: "content-preservation check failed",
            scope: Some("branch"),
            squad_id: None,
            guardian_id: Some(id),
            cell_id: None,
            task: None,
            log_path: None,
            payload: serde_json::json!({
                "branch": branch,
                "rebase_head": rebase_head,
                "lost_paths": lost,
            }),
            admin_only: false,
        });
    }
    // Poison the specific bad cache entries so a retry sees a real conflict
    // and goes through the agent instead of replaying the same lossy
    // resolution again.
    let mut forget_args = vec!["rerere", "forget"];
    forget_args.extend(lost.iter().map(String::as_str));
    let _ = wt.git(&forget_args);
    let _ = wt.git(&["rebase", "--abort"]);
    Err(format!(
        "a resolution would have dropped content in {lost:?} for branch {branch} (RAL-330 \
         content-preservation check) -- aborted rebase; a retry will resolve these paths with \
         the agent instead of replaying the same result"
    ))
}

/// RAL-330: whether `REBASE_HEAD`'s own patch (against its true parent) is
/// empty — i.e. this step genuinely has nothing left to contribute, the
/// legitimate case `--empty=drop`/`--skip` exist for. `true` when there is no
/// `REBASE_HEAD` to check (preserves [`advance_rebase`]'s prior behavior for
/// a state this function was never meant to gate).
fn rebase_head_commit_is_empty(wt: &Workspace) -> bool {
    match wt.git(&["rev-parse", "REBASE_HEAD"]) {
        Ok(sha) => {
            let sha = sha.trim();
            diff_numstat(wt, &[&format!("{sha}^"), sha]).is_empty()
        }
        Err(_) => true,
    }
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
///
/// RAL-330: even then, `--skip` only runs when `REBASE_HEAD`'s own commit is
/// confirmed empty. `--continue` can fail for reasons that have nothing to do
/// with the commit's content (a failing hook, a transient git error) while
/// the index is still fully staged for a *non-empty* commit; unconditionally
/// skipping in that case would silently discard the whole commit instead of
/// just the conflict `--skip` is meant to bypass. When the commit is not
/// confirmed empty, this leaves the rebase paused rather than guessing —
/// the caller's resolver loop will see it as still in progress and retry.
fn advance_rebase(wt: &Workspace) {
    if wt.git(&["rebase", "--continue"]).is_err()
        && conflicted_files(wt).is_empty()
        && rebase_head_commit_is_empty(wt)
    {
        let _ = wt.git(&["rebase", "--skip"]);
    }
}

/// The agent backend used to resolve conflicts: the review's own `stored` agent
/// (from `[[review]]`), else the `RALPHUS_RESOLVER_AGENT` env override, else
/// `.ralphus.toml`'s `[review].default_resolver_agent` (global layered under
/// `cwd`'s project config), else `"ollama"`.
pub(crate) fn resolver_agent(stored: Option<&str>, cwd: &Path) -> String {
    stored
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(ToString::to_string)
        .or_else(|| std::env::var("RALPHUS_RESOLVER_AGENT").ok())
        .unwrap_or_else(|| {
            crate::config::resolve(cwd)
                .default_resolver_agent()
                .to_string()
        })
}

/// The model the resolver runs: the review's own `stored` model, else the
/// `RALPHUS_RESOLVER_MODEL` env override, else `.ralphus.toml`'s
/// `[review].default_resolver_model` (global layered under `cwd`'s project
/// config), else `qwen3:8b` for the ollama backend. claude, claude-code, and
/// codex each pick their own default when still unset → `None`.
pub(crate) fn resolver_model(stored: Option<&str>, agent: &str, cwd: &Path) -> Option<String> {
    stored
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(ToString::to_string)
        .or_else(|| std::env::var("RALPHUS_RESOLVER_MODEL").ok())
        .or_else(|| {
            crate::config::resolve(cwd)
                .default_resolver_model()
                .map(ToString::to_string)
        })
        .or_else(|| match agent {
            "ollama" => Some("qwen3:8b".to_string()),
            _ => None, // codex, claude, claude-code: each picks its own default
        })
}

/// A review's resolver agent, resolved through `.ralphus.toml` custom agent
/// profiles (mirrors `scheduler::resolve_agent_selection`, which already does
/// this for `[[task.cell]].agent`) -- everything a `RunnerSpec` or
/// `chat_client::call_direct` needs to actually run it.
pub(crate) struct ResolvedResolverAgent {
    pub backend: String,
    pub executable: Option<String>,
    pub env: std::collections::BTreeMap<String, String>,
    pub custom_profile: bool,
    pub model: Option<String>,
}

/// Resolves a review's configured resolver agent (RAL-?) against configured
/// `.ralphus.toml` agent profiles before it is used for a runner spec or the
/// direct-chat fast path. `stored_agent`/`stored_model` are the guardian's own
/// `resolver_agent`/`resolver_model` columns; `cwd` is the worktree/project
/// path whose `.ralphus.toml` profiles apply.
///
/// Returns `Err` for a name that is neither a configured profile nor a
/// built-in backend -- callers must not build a `RunnerSpec` from that name;
/// see each call site's own error handling for how it surfaces this.
fn resolve_resolver_agent(
    stored_agent: Option<&str>,
    stored_model: Option<&str>,
    cwd: &Path,
) -> Result<ResolvedResolverAgent, String> {
    let raw = resolver_agent(stored_agent, cwd);
    let selection = crate::agent_profiles::resolve_agent_for_path(&raw, cwd)?;
    // Keyed off the *resolved* backend, not the raw profile name, so a custom
    // profile that resolves to `ollama` still gets the sensible `qwen3:8b`
    // default.
    let model = resolver_model(stored_model, &selection.backend, cwd);
    Ok(ResolvedResolverAgent {
        backend: selection.backend,
        executable: selection.executable,
        env: selection.env,
        custom_profile: selection.custom_profile,
        model,
    })
}

/// Resolves this review's resolver backend/model (RAL-149/168) -- cheap, one
/// DB read, no LLM call. Split out from [`resolve_conflicts_with_agent`] so
/// [`drive_rebase`]'s clean (no-conflict) path can also resolve it, without
/// paying for the (possibly LLM-backed) quality-bar synthesis unless a proof
/// call actually ends up running -- see [`proof_extras`].
fn resolver_backend(store: &Arc<Mutex<Store>>, id: &str) -> Result<ResolvedResolverAgent, String> {
    let guard = store.lock().expect("poisoned");
    let g = guard.get_guardian(id).map_err(|e| e.to_string())?;
    drop(guard);
    resolve_resolver_agent(
        g.resolver_agent.as_deref(),
        g.resolver_model.as_deref(),
        Path::new(&g.git_root),
    )
}

/// Reject an unavailable built-in resolver before a merge changes any review
/// worktree. Custom executable commands are intentionally skipped because they
/// may be compound shell commands and cannot be checked without executing them.
fn preflight_resolver_agent(
    runner: &dyn Runner,
    resolved: &ResolvedResolverAgent,
    machine: Option<&str>,
) -> Result<(), String> {
    runner
        .preflight_agent(&resolved.backend, resolved.executable.as_deref(), machine)
        .map_err(|error| format!("resolver agent unavailable: {error}"))
}

/// The effective environment one review branch's worktree runs under
/// (RAL-191): whatever the branch's *source cell* resolved to
/// (`squad < task < cell`), with the branch's own overrides and tombstones
/// applied on top.
///
/// A review worktree is assembled from a cell's work, so it inherits that
/// cell's environment by default — an agent resolving conflicts in the
/// worktree needs the same `API_URL`/`PATH`/toolchain variables the code was
/// written under, or it verifies against the wrong thing entirely. Per-branch
/// overrides then let a reviewer point one branch at a staging endpoint (or
/// drop a variable outright) without touching the original task.
///
/// Degrades to an empty map for an unknown branch rather than failing the
/// merge — the same posture every other guardian read here takes.
fn branch_env(
    store: &Arc<Mutex<Store>>,
    id: &str,
    branch_id: &str,
) -> std::collections::BTreeMap<String, String> {
    store
        .lock()
        .expect("poisoned")
        .resolve_guardian_branch_env(id, branch_id)
        .unwrap_or_default()
}

/// Quality-bar instructions + ghost-memory prefix for a branch's dedicated
/// final-proof call (RAL-149/168). Deliberately lazy: callers compute
/// this only once they've already decided [`run_final_proof`] will actually
/// run for this branch, since `synthesize_proof_instructions` may itself
/// invoke an LLM call -- under RAL-168's "nothing"/"final_branch" scopes (or
/// "each_branch" with auto-clean-skip), most branches never call this at all.
fn proof_extras(
    store: &Arc<Mutex<Store>>,
    id: &str,
    branch: &str,
    branch_id: &str,
    runner: &dyn Runner,
    resolved: &ResolvedResolverAgent,
    cancel: &CancelToken,
) -> (String, String) {
    let quality_note =
        synthesize_proof_instructions(store, id, branch, branch_id, runner, resolved, cancel);
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

/// Resolved "Proof" scope settings for one branch attempt (RAL-168): whether
/// and how often the dedicated LLM-based final-proof call
/// ([`run_final_proof`]) should fire, replacing the old `verify_mid_resolution`
/// flag outright. Computed once per branch from
/// `Guardian::effective_proof_scope`/`effective_proof_skip_auto_clean` (already
/// resolved against the project-level `.ralphus.toml` default) plus whether
/// this is the last branch in the stack.
#[derive(Debug, Clone)]
struct ProofGate {
    /// `"each_branch"` | `"final_branch"` | `"nothing"`.
    scope: String,
    /// Only meaningful under `"each_branch"`.
    skip_auto_clean: bool,
    /// Whether this branch is the last (by position) enabled branch in the
    /// stack -- the only branch `"final_branch"` scope proves.
    is_final_branch: bool,
}

impl ProofGate {
    /// Resolve a guardian's Proof-scope settings for one branch attempt,
    /// combining the guardian-level `effective_proof_scope`/
    /// `effective_proof_skip_auto_clean` with whether this particular
    /// branch is the last one in the stack.
    fn resolve(store: &Arc<Mutex<Store>>, id: &str, is_final_branch: bool) -> Self {
        let guard = store.lock().expect("poisoned");
        let g = guard.get_guardian(id).ok();
        ProofGate {
            scope: g
                .as_ref()
                .map(|g| g.effective_proof_scope.clone())
                .unwrap_or_else(|| "each_branch".to_string()),
            skip_auto_clean: g
                .as_ref()
                .is_some_and(|g| g.effective_proof_skip_auto_clean),
            is_final_branch,
        }
    }

    /// Whether [`run_final_proof`] should fire for a branch whose conflicts
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

    /// Whether [`run_final_proof`] should fire for a branch that rebased
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
/// in the stack" for `ProofScope::FinalBranch` (RAL-168; see
/// `OrderedBranch`'s doc comment on why position, not list order, is
/// authoritative). `None` for a guardian with no enabled branches.
fn final_branch_id(branches: &[crate::guardian::OrderedBranch]) -> Option<&str> {
    branches
        .iter()
        .filter(|b| b.enabled)
        .max_by_key(|b| b.position)
        .map(|b| b.id.as_str())
}

/// Record one guardian LLM call's cost as a line item (RAL-193) -- conflict
/// resolution, proving, chat, feedback, summary generation, etc. -- and,
/// if this review has a `maximum_budget_usd` cap, check whether its
/// cumulative cost (across every rebase/re-merge attempt) has now exceeded
/// it. Mirrors the RAL-161 cell/task budget kill-switch: once exceeded,
/// callers should stop making further resolver/prover calls and fail the
/// guardian/branch, using the returned error message.
fn record_guardian_call_cost(
    store: &Arc<Mutex<Store>>,
    id: &str,
    branch_id: Option<&str>,
    kind: &str,
    result: &crate::runner::RunnerResult,
) -> std::result::Result<(), String> {
    let guard = store.lock().expect("poisoned");
    let attempt = guard.guardian_current_attempt(id).unwrap_or(0);
    let _ = guard.record_guardian_cost(
        id,
        branch_id,
        attempt,
        kind,
        result.tokens_in,
        result.tokens_out,
        result.cost_usd,
    );
    let Ok(Some(cap)) = guard.guardian_maximum_budget_usd(id) else {
        return Ok(());
    };
    let Ok((_, _, cumulative)) = guard.guardian_cost_total(id) else {
        return Ok(());
    };
    if cumulative > cap {
        crate::rlog!(
            ERROR,
            "ralphus [guardian] review {id} cost ${cumulative:.4} exceeded maximum_budget_usd cap ${cap:.4}"
        );
        let _ = guard.cartographer_log(crate::cartographer::CartographerEntry {
            level: crate::logging::LogLevel::ERROR,
            source: "guardian",
            message: "review cost exceeded maximum_budget_usd cap",
            scope: Some("guardian"),
            squad_id: None,
            guardian_id: Some(id),
            cell_id: None,
            task: None,
            log_path: None,
            payload: serde_json::json!({"cumulative_cost_usd": cumulative, "cap": cap, "kind": kind}),
            admin_only: false,
        });
        return Err(format!(
            "review cost ${cumulative:.4} exceeded maximum_budget_usd cap ${cap:.4}"
        ));
    }
    Ok(())
}

/// Derives a concise quality-bar instruction for the conflict-resolver agent.
///
/// When the guardian has a task linkage, an LLM synthesises the raw proof steps
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
fn synthesize_proof_instructions(
    store: &Arc<Mutex<Store>>,
    id: &str,
    branch: &str,
    branch_id: &str,
    runner: &dyn Runner,
    resolved: &ResolvedResolverAgent,
    cancel: &CancelToken,
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

    let (explicit_checks, git_root, review_machine, proof_scope_is_nothing) = {
        let guard = store.lock().expect("poisoned");
        let checks = guard.guardian_checks(id).unwrap_or_default();
        let guardian = guard.get_guardian(id).ok();
        let root = guardian
            .as_ref()
            .map(|g| g.git_root.clone())
            .unwrap_or_default();
        let is_nothing = guardian
            .as_ref()
            .is_some_and(|g| g.effective_proof_scope == "nothing");
        let machine = guardian.and_then(|g| g.machine);
        (checks, root, machine, is_nothing)
    };

    // Opt-out: Proof scope is "nothing" -- the dedicated final-proof call
    // never fires for this branch (RAL-168's `ProofGate`), so the quality-bar
    // instructions built below would be handed to the resolver agent for
    // nothing (RAL-285: closes the gap where this ran regardless of scope).
    if proof_scope_is_nothing {
        return String::new();
    }

    let (cell_proofs, task_proofs) = {
        let guard = store.lock().expect("poisoned");
        match guard.proof_steps_for_review_branch(id, branch) {
            Ok(Some((sv, tv, _))) => (sv, tv),
            _ => (vec![], vec![]),
        }
    };

    let has_task_steps = !cell_proofs.is_empty() || !task_proofs.is_empty();
    let has_checks = !explicit_checks.is_empty();

    // No task linkage and no explicit checks → static project-discovery fallback.
    if !has_task_steps && !has_checks {
        log("no task proof steps or check commands found; using project-discovery fallback");
        return " After resolving, verify the code meets project quality standards: \
                check for a CLAUDE.md or AGENTS.md file in the repository root for build, \
                format, lint, and test instructions; run any applicable formatter and linter; \
                ensure the project builds without errors; then stage your changes."
            .to_string();
    }

    // No task linkage but explicit checks exist → keep the original format (no LLM call).
    if !has_task_steps {
        log("no task proof steps; using explicit check commands directly");
        return format!(
            " After resolving, your edits must keep these project checks passing: {}.",
            explicit_checks.join("; ")
        );
    }

    // Build the synthesis prompt from all available inputs.
    log(&format!(
        "synthesizing proof instructions from {} task + {} cell proof steps{}",
        task_proofs.len(),
        cell_proofs.len(),
        if has_checks {
            format!(" + {} explicit checks", explicit_checks.len())
        } else {
            String::new()
        },
    ));

    let mut lines: Vec<String> = Vec::new();
    if !task_proofs.is_empty() {
        lines.push("Task-level proof steps:".to_string());
        for v in &task_proofs {
            lines.push(format!("  [{}] {}", v.kind, v.spec));
        }
    }
    if !cell_proofs.is_empty() {
        lines.push("Cell-level proof steps:".to_string());
        for v in &cell_proofs {
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
        You are preparing proof instructions for a git rebase \
        conflict-resolution agent. The agent can run shell commands (formatters, \
        linters, tests) but CANNOT and MUST NOT commit, push, or abort the rebase \
        — the orchestrator handles those steps.\n\n\
        From the proof steps provided, produce a single concise paragraph (under \
        120 words) telling the agent what quality bar it must meet after resolving \
        conflicts. Rules:\n\
        - command-kind step: instruct the agent to run the command and fix any failures\n\
        - prompt-kind step: rephrase as a descriptive criterion (what \"done\" looks like)\n\
        - Deduplicate overlapping retry policies across steps — state each policy once\n\
        - Remove or adapt anything that conflicts with the rebase flow: no commit, \
          no push, no abort, no \"do not stage\", no task-failure side-effects\n\
        Output ONLY the instruction paragraph. No headers, labels, or commentary.";

    let spec = RunnerSpec {
        // RAL-102: squad_id/cell_id together key the tmux session name
        // (see `crate::tmux::session_name`) — must be unique per guardian so
        // concurrent guardians' agent invocations never collide on the same
        // tmux session.
        squad_id: format!("guardian-{id}"),
        task: "proof-synthesis".to_string(),
        cell_id: format!("synthesizer-{}", branch.replace(['/', '.'], "-")),
        cwd: git_root,
        prompt: Some(lines.join("\n")),
        command: None,
        agent: resolved.backend.clone(),
        executable: resolved.executable.clone(),
        model: resolved.model.clone(),
        system_prompt: Some(SYNTHESIS_SYSTEM.to_string()),
        system_prompt_position: None,
        timeout_sec: Some(120),
        budget_tokens: Some(1000),
        maximum_budget_usd: None,
        maximum_context: None,
        auto_compact_threshold: None,
        maximum_tool_output_tokens: None,
        proof: false,
        trace_context: None,
        resume_agent_session_id: None,
        assigned_agent_session_id: None,
        // RAL-191: this call only writes an instruction paragraph, but it runs
        // the branch's own agent -- keep it on the same environment as every
        // other per-branch invocation so a custom API base/proxy applies here
        // too. Profile env is the base layer; the branch's own overrides win.
        env_overrides: {
            let mut env = resolved.env.clone();
            env.extend(branch_env(store, id, branch_id));
            env
        },
        // RAL-201: this review may be assigned a remote machine even though
        // `git_root` here is a bare string, not a `Workspace` -- resolve it
        // from the guardian row directly rather than always defaulting local.
        machine: review_machine,
        tool_arg_truncate_chars: None,
        thrash_max_compactions: None,
        thrash_min_turn_gap: None,
        allow_personal_settings: false,
        allow_personal_memory: false,
    };
    let result = runner.run_cancellable(&spec, cancel);
    // RAL-193: not fatal from this helper (it returns a plain `String`, not a
    // `Result`) -- a budget already exceeded here is caught on the very next
    // call in `resolve_conflicts_with_agent`'s own loop.
    let _ = record_guardian_call_cost(store, id, Some(branch_id), "proof_synthesis", &result);

    if result.is_done() && !result.summary.trim().is_empty() {
        let synthesized = result.summary.trim().to_string();
        log(&format!(
            "proof synthesis complete ({} chars, in={} out={} tokens)",
            synthesized.len(),
            result.tokens_in,
            result.tokens_out,
        ));
        return format!(" After resolving and staging, meet this quality bar: {synthesized}");
    }

    // Synthesis LLM call failed — fall back gracefully.
    log(&format!(
        "proof synthesis failed ({}); falling back to {}",
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

/// Log + Cartographer-record a merge cancellation (RAL-213 cancellable
/// merges). Called from every checkpoint that bails out early once
/// `cancel.is_cancelled()` trips, so a real cancellation is always visible in
/// Cartographer alone, matching the nearby "merge starting"/"conflicts
/// resolved" entries' style. Deliberately does NOT touch guardian/branch
/// status or run any destructive git operation -- see the checkpoints' own
/// comments for why.
fn log_merge_cancelled(store: &Arc<Mutex<Store>>, id: &str) {
    crate::rlog!(INFO, "ralphus [guardian] review {id} merge cancelled");
    let guard = store.lock().expect("poisoned");
    let _ = guard.cartographer_log(crate::cartographer::CartographerEntry {
        level: crate::logging::LogLevel::INFO,
        source: "guardian",
        message: "merge cancelled",
        scope: Some("guardian"),
        squad_id: None,
        guardian_id: Some(id),
        cell_id: None,
        task: None,
        log_path: None,
        payload: serde_json::json!({}),
        admin_only: false,
    });
}

/// Shared tail for every "the rebase has no conflicts left" path in
/// [`resolve_conflicts_with_agent`] (mid-loop, and the post-loop recheck
/// after the attempt cap is hit): records the resolved conflict counts, logs
/// it, then runs (or skips, per `gate`) the dedicated final-proof pass.
#[allow(clippy::too_many_arguments)]
fn finish_branch_resolved(
    store: &Arc<Mutex<Store>>,
    id: &str,
    branch_id: &str,
    branch: &str,
    runner: &dyn Runner,
    wt: &Workspace,
    resolved: &ResolvedResolverAgent,
    gate: &ProofGate,
    cancel: &CancelToken,
    found: i64,
    committed: i64,
    last_session_id: Option<String>,
) -> std::result::Result<(Option<String>, String), String> {
    {
        let guard = store.lock().expect("poisoned");
        let _ = guard.set_guardian_conflicts(id, Some(found), Some(0), Some(committed));
        let _ = guard.set_branch_conflicts(id, branch_id, Some(found), Some(0), Some(committed));
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
            squad_id: None,
            guardian_id: Some(id),
            cell_id: None,
            task: None,
            log_path: None,
            payload: serde_json::json!({"branch": branch, "committed": committed}),
            admin_only: false,
        });
    }
    // RAL-168: gated by Proof scope -- a branch that just had real conflicts
    // resolved is never "auto-clean", so only `scope` (not `skip_auto_clean`)
    // matters here.
    if !gate.allows_after_conflict() {
        crate::rlog!(
            INFO,
            "ralphus [guardian] review {id} final proof skipped branch={branch:?} scope={:?}",
            gate.scope
        );
        return Ok((
            last_session_id,
            "resolved by agent; final proof skipped (Proof scope)".to_string(),
        ));
    }
    let (quality_note, ghost_prefix) =
        proof_extras(store, id, branch, branch_id, runner, resolved, cancel);
    let (proof_session_id, proof_detail) = run_final_proof(
        store,
        id,
        branch_id,
        runner,
        wt,
        branch,
        resolved,
        &quality_note,
        &ghost_prefix,
        cancel,
    );
    Ok((proof_session_id.or(last_session_id), proof_detail))
}

/// How many resolution passes a single conflicting commit gets before the
/// branch gives up on it. The second (and any later) pass resumes the same
/// agent session as the first (see `current_commit_session_id` below) rather
/// than starting cold, so this isn't "try 2 unrelated times" -- it's "let the
/// agent take one follow-up swing with everything it already knows about this
/// commit's conflicts."
const MAX_ATTEMPTS_PER_COMMIT: u32 = 2;

/// Give up resolving the commit the rebase is currently stopped on, after it
/// used up its [`MAX_ATTEMPTS_PER_COMMIT`]-pass budget. Everything logged here
/// is deterministic from the worktree's own state at this exact moment -- the
/// caller runs `git rebase --abort` the instant this returns, which makes the
/// worktree look clean/finished as if nothing had failed, so this is the last
/// chance to record what was actually still broken.
fn give_up_on_stuck_commit(
    store: &Arc<Mutex<Store>>,
    id: &str,
    branch: &str,
    wt: &Workspace,
    found: i64,
    committed: i64,
    attempts: u32,
) -> String {
    let final_files = conflicted_files(wt);
    let final_remaining = i64::try_from(count_markers(wt, &final_files)).unwrap_or(i64::MAX);
    let in_progress = rebase_in_progress(wt);
    let rebase_head = wt
        .git(&["rev-parse", "REBASE_HEAD"])
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());
    // `rebase_command_progress` (git's own todo-list done/total, read from
    // `rebase-merge/done` and `rebase-merge/git-rebase-todo`) is captured here,
    // before the caller's abort runs, so the log states outright how much of
    // the stack was left rather than leaving that to be reconstructed after
    // the fact from whatever the worktree happens to look like post-cleanup.
    let command_progress = rebase_command_progress(wt);
    crate::rlog!(
        WARNING,
        "ralphus [guardian] review {id} conflict resolver exhausted its \
         {attempts}/{MAX_ATTEMPTS_PER_COMMIT}-attempt budget on this commit \
         branch={branch:?} found={found} committed={committed} \
         remaining_files={final_files:?} remaining_markers={final_remaining} \
         rebase_in_progress={in_progress} rebase_head={rebase_head:?} \
         rebase_commands_done_of_total={command_progress:?} -- about to run \
         `git rebase --abort`, which will make the worktree look clean/finished \
         afterward even though it is not"
    );
    {
        let guard = store.lock().expect("poisoned");
        let _ = guard.cartographer_log(crate::cartographer::CartographerEntry {
            level: crate::logging::LogLevel::WARNING,
            source: "guardian",
            message: "conflict resolver exhausted its attempt budget on this commit",
            scope: Some("branch"),
            squad_id: None,
            guardian_id: Some(id),
            cell_id: None,
            task: None,
            log_path: None,
            admin_only: false,
            payload: serde_json::json!({
                "branch": branch,
                "found": found,
                "committed": committed,
                "attempts": attempts,
                "max_attempts": MAX_ATTEMPTS_PER_COMMIT,
                "remaining_files": final_files,
                "remaining_markers": final_remaining,
                "rebase_in_progress": in_progress,
                "rebase_head": rebase_head,
                "rebase_commands_done": command_progress.map(|(done, _)| done),
                "rebase_commands_total": command_progress.map(|(_, total)| total),
            }),
        });
    }
    "conflict resolver exhausted its attempt budget on this commit".to_string()
}

/// Drive an agent to resolve the in-progress rebase conflicts in `wt`, then
/// `git add` + `rebase --continue`, looping until the rebase completes. Each
/// conflicting commit gets up to [`MAX_ATTEMPTS_PER_COMMIT`] resolution passes
/// before the branch gives up on that commit specifically -- not a single cap
/// shared across the whole branch's rebase, so a long stack of clean commits
/// can't burn through the same budget a single stuck conflict needs. Once
/// every conflict marker is resolved and committed, runs a dedicated
/// final-proof agent call (RAL-149) before returning -- see
/// [`run_final_proof`]. Returns `Ok((agent_session_id, proof_detail))` when
/// fully resolved and proofed (pass or fail; the proof call never blocks the
/// rebase from completing -- see [`run_final_proof`]'s doc comment).
#[allow(clippy::too_many_arguments)]
fn resolve_conflicts_with_agent(
    store: &Arc<Mutex<Store>>,
    id: &str,
    branch_id: &str,
    runner: &dyn Runner,
    wt: &Workspace,
    branch: &str,
    resolved: &ResolvedResolverAgent,
    gate: &ProofGate,
    cancel: &CancelToken,
) -> std::result::Result<(Option<String>, String), String> {
    let agent = resolved.backend.clone();
    let model = resolved.model.clone();

    // RAL-136: ghost memory for review worktrees. Reviews aren't part of the
    // task dependency graph (Q2's "one level up" lookup is task-graph only),
    // so the only context to inject here is this branch's *own* prior ghost --
    // e.g. from an earlier resolve pass or a rebuild after the base branch
    // shifted (`rebuild_on_base_shift`). The URI is computed once here and
    // reused both by the per-iteration read below (inside the loop) and by
    // the `guard.upsert_ghost(&ghost_uri, ...)` write further down.
    let ghost_uri = crate::ghost::review_uri(id, Some(branch_id));

    // Seed the live progress (RAL-72): found = current conflicting commit's marker
    // block count, fixed = files resolved in working tree (not staged), committed =
    // hunks staged for the *current* conflicting commit. found, committed,
    // commit_attempts, and current_commit_session_id are all scoped to whichever
    // commit the rebase is presently stopped on: found is recomputed fresh from
    // disk every loop iteration below, and the other three reset every time the
    // rebase advances to its next commit (RAL-144) -- none of them accumulate
    // across commits within the branch's rebase.
    let mut found = i64::try_from(count_markers(wt, &conflicted_files(wt))).unwrap_or(i64::MAX);
    let mut committed = 0i64;
    let mut last_session_id: Option<String> = None;
    // Resolution passes used on the commit currently being worked on, capped at
    // MAX_ATTEMPTS_PER_COMMIT.
    let mut commit_attempts: u32 = 0;
    // The resolver's own agent session for the commit currently being worked
    // on, so a retry resumes that conversation instead of starting cold. Reset
    // to `None` whenever the rebase advances to a new commit.
    let mut current_commit_session_id: Option<String> = None;
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
            squad_id: None,
            guardian_id: Some(id),
            cell_id: None,
            task: None,
            log_path: None,
            payload: serde_json::json!({
                "branch": branch,
                "found": found,
                "agent": agent,
                "model": model,
            }),
            admin_only: false,
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

    loop {
        if cancel.is_cancelled() {
            log_merge_cancelled(store, id);
            return Err("cancelled".to_string());
        }
        let files = conflicted_files(wt);
        if files.is_empty() {
            // No conflicts left: the rebase has either finished or auto-advanced
            // through clean commits. If it is still in progress, drive it forward;
            // once it reports no rebase in progress we are done.
            if rebase_in_progress(wt) {
                // RAL-330: before trusting this step's already-staged resolution
                // (most commonly rerere's fast path, which never goes through
                // this function's own agent call at all), verify it didn't
                // silently drop real content. `found == 0` here only means "git
                // has no unmerged files right now" -- true either because the
                // conflict was resolved correctly, or because whatever resolved
                // it made the diff disappear entirely.
                guard_against_rebase_step_content_loss(store, id, branch, wt)?;
                advance_rebase(wt);
                // RAL-144: advancing to the next commit -- nothing was found or
                // committed for it yet.
                committed = 0;
                commit_attempts = 0;
                current_commit_session_id = None;
                continue;
            }
            return finish_branch_resolved(
                store,
                id,
                branch_id,
                branch,
                runner,
                wt,
                resolved,
                gate,
                cancel,
                found,
                committed,
                last_session_id,
            );
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
                    squad_id: None,
                    guardian_id: Some(id),
                    cell_id: None,
                    task: None,
                    log_path: None,
                    payload: serde_json::json!({"branch": branch, "files": files.len()}),
                    admin_only: false,
                });
            }
            wt.git(&["add", "--all"])?;
            guard_against_rebase_step_content_loss(store, id, branch, wt)?;
            advance_rebase(wt);
            // RAL-144: advancing to the next commit -- nothing committed for it yet.
            committed = 0;
            commit_attempts = 0;
            current_commit_session_id = None;
            continue;
        }

        // RAL-136: re-read the ghost note fresh on every iteration (rather than
        // once before the loop) so a retry within this same call sees whatever
        // an earlier iteration just published via `guard.upsert_ghost` below,
        // instead of a stale pre-loop snapshot.
        let ghost_prefix = {
            let guard = store.lock().expect("poisoned");
            guard
                .get_ghost(&ghost_uri)
                .ok()
                .flatten()
                .and_then(|g| crate::ghost::format_context_block(Some(&g), &[]))
                .unwrap_or_default()
        };
        let prompt = format!(
            "{ghost_prefix}Resolve all merge conflict markers in these files from branch '{branch}': {}. \
             Read each file, intelligently merge both sides of every conflict block \
             (<<<<<<<...=======...>>>>>>>), and write the resolved content back with \
             ALL markers removed.",
            files.join(", ")
        );
        // RAL-168: this fix pass never runs formatters/linters/tests, even if
        // the reviewer's own project uses them heavily -- that responsibility
        // belongs solely to the dedicated final-proof call
        // (`run_final_proof`/[`ProofGate`] above), which already handles it
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
             proof pass runs afterward and will handle formatting/linting/testing, \
             including auto-fixing any failures it finds. Do NOT call `git rebase --continue`, \
             `git commit`, `git push`, or any other git command besides `git add -A`. The \
             orchestrator advances the rebase as soon as it sees RALPHUS_STAGE: DONE in your \
             output.";
        // This is either the commit's first pass (commit_attempts was reset to
        // 0 the last time the rebase advanced) or a retry -- in which case
        // current_commit_session_id carries the previous pass's session so the
        // agent resumes its own conversation instead of starting cold. Count
        // only a completed agent pass below: a runner/backend invocation that
        // cannot produce a result is not an attempt to resolve conflicts.
        let spec = RunnerSpec {
            // RAL-102: unique per (guardian, branch) so the tmux session this
            // resolves through (see `crate::tmux::session_name`) never
            // collides with another guardian's or branch's resolver.
            // RAL-192: keyed on the branch's stable id, not its mutable stack
            // position, so the Review tab's capture-pane endpoint (and the
            // historical-record snapshot lookup) can still address this exact
            // invocation via the same (guardian id, branch id) pair even
            // after a later reorder/add/remove shifts positions.
            squad_id: format!("guardian-{id}"),
            task: RESOLVER_TASK.to_string(),
            cell_id: format!("resolver-{branch_id}"),
            cwd: wt.root().to_string_lossy().into_owned(),
            prompt: Some(prompt),
            command: None,
            agent: agent.clone(),
            executable: resolved.executable.clone(),
            model: model.clone(),
            system_prompt: Some(system_prompt.to_string()),
            system_prompt_position: None,
            timeout_sec: None,
            budget_tokens: None,
            maximum_budget_usd: None,
            maximum_context: None,
            auto_compact_threshold: None,
            maximum_tool_output_tokens: None,
            proof: false,
            trace_context: None,
            resume_agent_session_id: current_commit_session_id.clone(),
            assigned_agent_session_id: None,
            // RAL-191: the resolver edits this branch's own worktree, so it
            // runs under the branch's resolved environment. Profile env is
            // the base layer; the branch's own overrides win.
            env_overrides: {
                let mut env = resolved.env.clone();
                env.extend(branch_env(store, id, branch_id));
                env
            },
            // RAL-201: without this, the resolver agent always ran on the
            // daemon's own host even when `wt` (and every git/fs op this
            // function performs on it) is on a review's assigned remote
            // machine -- a mismatch that fails loudly (the local runner
            // cannot find a `cwd` that only exists on another host) rather
            // than silently running in the wrong place, but is still wrong.
            machine: wt.machine().map(str::to_string),
            tool_arg_truncate_chars: None,
            thrash_max_compactions: None,
            thrash_min_turn_gap: None,
            allow_personal_settings: false,
            allow_personal_memory: false,
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

        // Spawn a second thread that re-scans the worktree for remaining
        // conflict markers every couple seconds while the agent call below is
        // in flight. `count_markers`/`conflicted_files` are plain git+filesystem
        // reads -- they need no cooperation from the agent, since it is editing
        // files on disk in this same worktree the whole time -- so
        // `conflicts_fixed` can track real progress instead of sitting frozen
        // at 0 until the (possibly many-minute) call returns.
        let marker_stop = Arc::new(AtomicBool::new(false));
        let marker_stop_clone = Arc::clone(&marker_stop);
        let marker_store = Arc::clone(store);
        let marker_wt = wt.clone();
        let marker_files = files.clone();
        let marker_id = id.to_string();
        let marker_branch_id = branch_id.to_string();
        let marker_watcher = std::thread::spawn(move || {
            while !marker_stop_clone.load(Ordering::Relaxed) {
                std::thread::sleep(std::time::Duration::from_millis(2000));
                if marker_stop_clone.load(Ordering::Relaxed) {
                    break;
                }
                let remaining =
                    i64::try_from(count_markers(&marker_wt, &marker_files)).unwrap_or(i64::MAX);
                let fixed = markers_before.saturating_sub(remaining).max(0);
                let guard = marker_store.lock().expect("poisoned");
                let _ = guard.set_guardian_conflicts(
                    &marker_id,
                    Some(found),
                    Some(fixed),
                    Some(committed),
                );
                let _ = guard.set_branch_conflicts(
                    &marker_id,
                    &marker_branch_id,
                    Some(found),
                    Some(fixed),
                    Some(committed),
                );
            }
        });

        // RAL-259: the resolver agent is actually beginning to run — stamp the
        // branch's Live-View start time (COALESCE so the fix pass, fired first
        // within this attempt, wins over the final-proof call that may follow).
        let _ = store
            .lock()
            .expect("poisoned")
            .stamp_branch_started_at(id, branch_id);
        let result = runner.run_cancellable(&spec, cancel);

        stop.store(true, Ordering::Relaxed);
        let _ = watcher.join();
        marker_stop.store(true, Ordering::Relaxed);
        let _ = marker_watcher.join();

        record_guardian_call_cost(store, id, Some(branch_id), "resolve_conflict", &result)?;

        // RAL-213: the runner call above may have blocked until `cancel`
        // tripped (a real subprocess is killed the same way) -- check again
        // now, before treating a resulting non-done result as a genuine
        // resolver failure, so a cancellation never gets misreported as
        // "conflict resolver failed" and never trips the caller's abort path.
        if cancel.is_cancelled() {
            log_merge_cancelled(store, id);
            return Err("cancelled".to_string());
        }

        if let Some(sid) = result.agent_session_id.clone() {
            last_session_id = Some(sid.clone());
            current_commit_session_id = Some(sid);
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
                    squad_id: None,
                    guardian_id: Some(id),
                    cell_id: None,
                    task: None,
                    log_path: None,
                    payload: serde_json::json!({"branch": branch, "error": err}),
                    admin_only: false,
                });
            }
            return Err(format!("conflict resolver invocation failed: {err}"));
        }

        commit_attempts += 1;

        // RAL-136: persist the resolver's self-summarized handoff note, if it
        // produced one (same `RALPHUS_GHOST:` marker/system-prompt path as a
        // task cell -- see `cli/src/ralphus/runner/execute.py`). Merges
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
                        squad_id: None,
                        guardian_id: Some(id),
                        cell_id: None,
                        task: None,
                        log_path: None,
                        payload: serde_json::json!({"branch": branch, "len": ghost_text.len()}),
                        admin_only: false,
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
                    squad_id: None,
                    guardian_id: Some(id),
                    cell_id: None,
                    task: None,
                    log_path: None,
                    payload: serde_json::json!({"branch": branch, "committed": committed}),
                    admin_only: false,
                });
            }
            guard_against_rebase_step_content_loss(store, id, branch, wt)?;
            advance_rebase(wt);
            // RAL-144: advancing to the next commit -- nothing committed for it yet.
            committed = 0;
            commit_attempts = 0;
            current_commit_session_id = None;
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
            // Markers still present. Logged (not just silently retried) because
            // this is exactly the kind of per-pass detail that's otherwise
            // invisible until the commit finally resolves or exhausts its
            // attempt budget -- these counts are all deterministic right here,
            // no reason to only find out post hoc.
            crate::rlog!(
                WARNING,
                "ralphus [guardian] review {id} conflict resolver pass incomplete \
                 branch={branch:?} markers_before={markers_before} fixed={fixed} \
                 remaining={remaining} files={files:?}"
            );
            {
                let guard = store.lock().expect("poisoned");
                let _ = guard.cartographer_log(crate::cartographer::CartographerEntry {
                    level: crate::logging::LogLevel::WARNING,
                    source: "guardian",
                    message: "conflict resolver pass incomplete, retrying",
                    scope: Some("branch"),
                    squad_id: None,
                    guardian_id: Some(id),
                    cell_id: None,
                    task: None,
                    log_path: None,
                    payload: serde_json::json!({
                        "branch": branch,
                        "markers_before": markers_before,
                        "fixed": fixed,
                        "remaining": remaining,
                        "files": files,
                    }),
                    admin_only: false,
                });
            }
            if commit_attempts >= MAX_ATTEMPTS_PER_COMMIT {
                return Err(give_up_on_stuck_commit(
                    store,
                    id,
                    branch,
                    wt,
                    found,
                    committed,
                    commit_attempts,
                ));
            }
            // The agent may need a follow-up pass to fully clear all conflicts
            // (e.g. partial resolution or a multi-file case) -- resuming its own
            // session (current_commit_session_id was set above) rather than
            // starting cold.
            continue;
        }
        wt.git(&["add", "--all"])?;
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
        guard_against_rebase_step_content_loss(store, id, branch, wt)?;
        advance_rebase(wt);
        // RAL-144: advancing to the next commit -- nothing committed for it yet.
        committed = 0;
        commit_attempts = 0;
        current_commit_session_id = None;
    }
}

/// RAL-149/168: dedicated final-proof agent call. Runs once a
/// branch's conflict markers are all resolved and committed (from
/// `resolve_conflicts_with_agent`'s loop above), or -- per RAL-168's Proof
/// scope -- against a branch that rebased cleanly with no conflict at all
/// (from `drive_rebase` directly) -- a separate LLM call from the fix pass so
/// the board's "final proof pending" indicator reflects a real,
/// distinct step rather than something bundled into the fix call. Its system
/// prompt does not assume a conflict occurred, so it inspects the worktree
/// itself rather than assuming what state the code is in.
///
/// Sets the branch's merge status to [`MergeStatus::ProofPending`] for the
/// call's duration -- the caller clears it (to `conflict_resolved`) once this
/// returns. Reuses the `proof: true` `RunnerSpec` contract (same
/// `RALPHUS_PROOF: PASS/FAIL` marker-parsing, fail-closed on no verdict) that
/// `agent`-kind task proof steps already use.
///
/// Never fails the branch: like the quality-bar instructions it carries, this
/// call is advisory. A FAIL verdict (or a runner error) is folded into the
/// returned detail message for a reviewer to see, not treated as a rebase
/// failure -- a proof retry/blocking policy is explicitly out of scope for
/// RAL-149.
#[allow(clippy::too_many_arguments)]
fn run_final_proof(
    store: &Arc<Mutex<Store>>,
    id: &str,
    branch_id: &str,
    runner: &dyn Runner,
    wt: &Workspace,
    branch: &str,
    resolved: &ResolvedResolverAgent,
    quality_note: &str,
    ghost_prefix: &str,
    cancel: &CancelToken,
) -> (Option<String>, String) {
    let agent = &resolved.backend;
    let model = &resolved.model;
    {
        let guard = store.lock().expect("poisoned");
        let _ = guard.set_branch_status(id, branch_id, MergeStatus::ProofPending, None);
        let _ = guard.cartographer_log(crate::cartographer::CartographerEntry {
            level: crate::logging::LogLevel::INFO,
            source: "guardian",
            message: "final proof starting",
            scope: Some("branch"),
            squad_id: None,
            guardian_id: Some(id),
            cell_id: None,
            task: None,
            log_path: None,
            payload: serde_json::json!({"branch": branch}),
            admin_only: false,
        });
    }
    crate::rlog!(
        INFO,
        "ralphus [guardian] review {id} final proof starting branch={branch:?} agent={agent:?} model={model:?}"
    );

    let prompt = format!(
        "{ghost_prefix}Confirm branch '{branch}' is ready in this worktree, after being \
         rebased onto the current stack -- whether or not that rebase hit a conflict, an \
         earlier fix pass may or may not have made changes here to satisfy the project's \
         quality bar, so do not assume what state the code is in; inspect it yourself.{quality_note}"
    );
    let system_prompt = "You are running the dedicated final-proof pass of a git rebase \
         conflict-resolution cycle, in a checked-out worktree. Confirm the code meets the \
         quality bar described in the prompt, fixing anything you reasonably can. If you edit \
         any files, run `git add -A` with run_bash to stage them before you finish. Do NOT call \
         `git rebase --continue`, `git commit`, `git push`, `git rebase --abort`, or any other \
         rebase-affecting git command -- the orchestrator owns the rebase and has already \
         advanced past the conflict this branch was resolving.";
    let spec = RunnerSpec {
        // RAL-192: keyed on the branch's stable id (not its mutable stack
        // position -- see `crate::tmux::session_name`'s doc comment) so a
        // reorder/add/remove elsewhere in the review never breaks the
        // historical-record lookup for this cell. Mirrors the fix pass's
        // `resolver-{branch_id}` cell id (RAL-102) -- distinct so the two
        // calls never collide on the same tmux session.
        squad_id: format!("guardian-{id}"),
        task: RESOLVER_PROOF_TASK.to_string(),
        cell_id: format!("resolver-proof-{branch_id}"),
        cwd: wt.root().to_string_lossy().into_owned(),
        prompt: Some(prompt),
        command: None,
        agent: agent.to_string(),
        executable: resolved.executable.clone(),
        model: model.clone(),
        system_prompt: Some(system_prompt.to_string()),
        system_prompt_position: None,
        timeout_sec: None,
        budget_tokens: None,
        maximum_budget_usd: None,
        maximum_context: None,
        auto_compact_threshold: None,
        maximum_tool_output_tokens: None,
        proof: true,
        trace_context: None,
        resume_agent_session_id: None,
        assigned_agent_session_id: None,
        // RAL-191: same worktree, same environment as the fix pass above.
        // Profile env is the base layer; the branch's own overrides win.
        env_overrides: {
            let mut env = resolved.env.clone();
            env.extend(branch_env(store, id, branch_id));
            env
        },
        // RAL-201: route to the same machine `wt` is actually on -- see the
        // identical fix in `resolve_conflicts_with_agent` above.
        machine: wt.machine().map(str::to_string),
        tool_arg_truncate_chars: None,
        thrash_max_compactions: None,
        thrash_min_turn_gap: None,
        allow_personal_settings: false,
        allow_personal_memory: false,
    };
    // RAL-259: the final-proof agent is actually beginning to run — stamp the
    // branch's Live-View start time. COALESCE means a branch that already
    // started a fix pass keeps that (earlier) start; one that went straight to
    // proof (clean rebase) gets stamped here.
    let _ = store
        .lock()
        .expect("poisoned")
        .stamp_branch_started_at(id, branch_id);
    let result = runner.run_cancellable(&spec, cancel);
    // RAL-193: not fatal here -- per this function's own doc comment, the
    // proof call never blocks the rebase from completing, so a budget overrun is
    // recorded but doesn't abort an already-in-flight resolution.
    let _ = record_guardian_call_cost(store, id, Some(branch_id), "proof", &result);
    let passed = result.proof_passed();

    crate::rlog!(
        INFO,
        "ralphus [guardian] review {id} final proof done branch={branch:?} passed={passed}"
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
            message: "final proof done",
            scope: Some("branch"),
            squad_id: None,
            guardian_id: Some(id),
            cell_id: None,
            task: None,
            log_path: None,
            payload: serde_json::json!({"branch": branch, "passed": passed}),
            admin_only: false,
        });
    }

    let detail = if passed {
        "resolved by agent; final proof passed".to_string()
    } else {
        let reason = result
            .error
            .as_deref()
            .filter(|s| !s.trim().is_empty())
            .or_else(|| Some(result.summary.trim()).filter(|s| !s.is_empty()))
            .unwrap_or("no verdict reported");
        format!("resolved by agent; final proof failed: {reason}")
    };
    (result.agent_session_id, detail)
}

/// CCTL-134: run the review's check gates against a single stacked commit's
/// worktree. Returns `Err` with the failing command on the first failure. A
/// review that opted out of checks (CCTL-130) or declares none passes trivially.
///
/// On a full pass, RAL-152 folds a ground-truth "this was validated" note
/// onto `branch_id`'s own ghost, mirroring what `run_proofs`/
/// `note_proof_outcome` do for task cells in `scheduler.rs` — a resolver
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
    cancel: &CancelToken,
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
    // RAL-191: check gates run under the branch's resolved environment, same as
    // the agent invocations against this worktree -- a gate like `cargo test`
    // is worthless if it runs without the variables the code expects.
    let env = branch_env(store, id, branch_id);
    for cmd in &checks {
        // RAL-239: a review cancelled while a check gate is running must not
        // let the next queued check start against this worktree.
        if cancel.is_cancelled() {
            return Err("cancelled".to_string());
        }
        if !wt.run_command_with_env(cmd, &env, cancel).0 {
            return Err(format!("check failed after '{branch}': {cmd}"));
        }
    }
    let wt_str = wt.root().to_string_lossy().into_owned();
    if !checks.is_empty() {
        let uri = crate::ghost::review_uri(id, Some(branch_id));
        let note = crate::ghost::proof_outcome_note(checks.len(), checks.len());
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

/// Deterministic short worktree-directory names (RAL-211) for every currently
/// enabled branch a guardian is building in `project` (or, when `project` is
/// `None`, every enabled branch regardless of project -- used by the
/// feedback-routing path, which only ever operates on single-project
/// guardians and resolves `wt_base` from the guardian's primary `git_root`
/// under that same assumption).
///
/// Computed fresh from the store's current branch rows every time, in
/// position order, rather than threaded through as state: as long as every
/// call site filters the same way (enabled, same project), the same branch
/// set in the same order always produces the same short names -- which is
/// what makes a worktree already on disk get found again by a later restack
/// or feedback pass instead of silently retargeted at the wrong branch.
fn branch_short_names(
    store: &Arc<Mutex<Store>>,
    id: &str,
    project: Option<&str>,
) -> std::collections::HashMap<String, String> {
    let guardian = store.lock().expect("poisoned").get_guardian(id).ok();
    let git_root = guardian
        .as_ref()
        .map(|g| g.git_root.clone())
        .unwrap_or_default();
    let names: Vec<String> = guardian
        .map(|g| g.branches)
        .unwrap_or_default()
        .into_iter()
        .filter(|b| b.enabled)
        .filter(|b| match project {
            Some(p) => b.project.as_deref().unwrap_or(&git_root) == p,
            None => true,
        })
        .map(|b| b.branch)
        .collect();
    crate::short_paths::dedupe_short_names(names.iter().map(String::as_str))
}

/// The per-branch review worktree directory under `wt_base` for `branch`,
/// using its RAL-211 short name from `short_names` when one was computed for
/// it (always true for an enabled branch in the same project `short_names`
/// was built from); falls back to an un-deduped short name otherwise, which
/// only happens for a branch outside that scope and therefore can never
/// collide with one that IS in scope.
fn branch_wt_dir(
    wt_base: &Workspace,
    short_names: &std::collections::HashMap<String, String>,
    branch: &str,
) -> Workspace {
    let short = short_names
        .get(branch)
        .cloned()
        .unwrap_or_else(|| crate::short_paths::short_name(branch));
    wt_base.join(format!("wt-{short}"))
}

/// Re-stack every branch whose `position > from_position` onto the review branch
/// at `from_position` (which is assumed to already have the desired HEAD). Runs
/// check gates on each branch; finalises the combined worktree at the end.
///
/// Used by [`pull_pr_commits`] and [`rebase_on_manual_push`] to apply the
/// downstream-rebase logic after a change lands on one branch outside a normal
/// merge pass. [`run_feedback`] applies the same logic via its own inline copy
/// of this loop rather than calling this helper.
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
    cancel: &CancelToken,
) {
    // Re-affirm `Merging` here rather than trusting the caller's earlier stamp:
    // both callers (`run_feedback`, `rebase_on_manual_push`) set it once before
    // dispatching a potentially slow resolver-agent call, and a concurrent
    // failure elsewhere (e.g. a racing "Merge / rebase" click hitting a
    // transient worktree error) can overwrite the guardian to `MergeFailed` in
    // that window. Without this, nothing corrects it back for the rest of this
    // restack -- every branch below keeps advancing to `done`/`conflict_resolved`
    // while the top-level status stays stuck on the stale failure until
    // `finalize_review` finally overwrites it at the very end.
    set_status(GuardianStatus::Merging, None);
    // RAL-103: this is a forced regeneration (feedback routing or a detected
    // manual push) -- clear the stale manual-checks commands up front so
    // `checks_state` drops out of "ready" for the whole restack, instead of
    // showing the previous build's commands as current until
    // `generate_manual_commands` overwrites them at the end.
    let _ = store
        .lock()
        .expect("poisoned")
        .clear_guardian_manual_commands(id);
    // RAL-193: this restack is its own re-merge attempt -- a distinct
    // resolver/prover cost bucket from whatever attempt preceded it,
    // whether triggered by routed reviewer feedback or a detected manual
    // push (both call this, not `run_merge`).
    let _ = store
        .lock()
        .expect("poisoned")
        .bump_guardian_merge_attempt(id);
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
    // RAL-211: short worktree-directory names, computed once for this pass --
    // see `branch_short_names`'s doc comment for why this must stay
    // consistent with every other call site's computation for the same
    // guardian.
    let short_names = branch_short_names(store, id, None);
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
    // RAL-168: unambiguous "last branch in the stack" for `ProofScope::FinalBranch`,
    // computed once against the FULL branch list (not the `from_position`-filtered
    // re-stack subset below) -- a re-stack starting mid-stack must still recognize
    // the true final branch even when it isn't touched by this particular pass.
    let final_id = final_branch_id(&branches).map(str::to_string);
    let mut prev_ref = branches
        .iter()
        .find(|b| b.position == from_position)
        .map(|b| review_ref_of_ordered(id, b))
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
        let rev = match claim_branch_review_ref(
            store,
            root,
            id,
            &root.root().to_string_lossy(),
            &ob.id,
            &ob.branch,
            ob.readable_review_branch,
            ob.review_branch_name.as_deref(),
        ) {
            Ok(rev) => rev,
            Err(e) => {
                fail_branch(store, id, &ob.id, &ob.branch, &e, set_status);
                return;
            }
        };
        let wt_j = branch_wt_dir(wt_base, &short_names, &ob.branch);
        let wt_j_str = wt_j.root().to_string_lossy().to_string();
        if let Err(e) = worktree_add_or_reset(root, &rev, &wt_j, &ob.branch) {
            fail_branch(store, id, &ob.id, &ob.branch, &e, set_status);
            return;
        }
        let _ = store
            .lock()
            .expect("poisoned")
            .set_branch_review(id, &ob.id, &rev, &wt_j_str);
        let gate = ProofGate::resolve(store, id, Some(&ob.id) == final_id.as_ref());
        if stack_pick(
            store, runner, id, &ob.id, &ob.branch, &base_sha, &prev_ref, &rev, &wt_j, squash,
            &gate, cancel,
        )
        .is_err()
        {
            return;
        }
        if let Err(e) = run_commit_checks(store, id, &ob.id, &wt_j, &ob.branch, cancel) {
            fail_branch(store, id, &ob.id, &ob.branch, &e, set_status);
            return;
        }
        prev_ref = rev;
    }
    match finalize_review(store, runner, root, wt_base, id, &prev_ref, cancel) {
        Ok(note) => {
            // RAL-208: request a change-summary regen -- a no-op unless the
            // enabled-branch set actually changed since the last one (a plain
            // re-stack never does), and debounced when it did.
            queue_final_summary_regen(store, id);
            // RAL-27: regenerate manual review commands after re-stacking.
            generate_manual_commands(
                store,
                runner,
                id,
                root,
                &base_sha,
                &prev_ref,
                Some(&wt_base.join("review")),
                cancel,
            );
            // RAL-92: re-baseline every branch's review-branch tip now that the
            // stack has settled, so the restacked downstream branches are not
            // mistaken for a manual push on the next maintenance sweep.
            snapshot_review_heads(store, id);
            set_status(GuardianStatus::InReview, note.as_deref());
        }
        Err(e) => set_status(GuardianStatus::MergeFailed, Some(&e)),
    }
}

/// The worktree directory a guardian's review stack is built in:
/// `.git/.ralphus/g/g<n>`, `<n>` the guardian's numeric id with zero-padding
/// stripped (see `crate::short_paths`).
///
/// Lives inside `.git/` (which git already excludes from the working tree) so
/// the review worktrees never appear at the repo root and need no gitignore
/// entry. Git worktrees checked out under `.git` resolve normally — the admin
/// entry in `.git/worktrees/<name>` and the `commondir` back-pointer are
/// independent of where the checkout itself lives. Git branch names are
/// unaffected -- only this directory name is shortened.
pub(crate) fn worktree_dir(git_root: &str, guardian_id: &str) -> PathBuf {
    crate::short_paths::ralphus_root(Path::new(git_root))
        .join("g")
        .join(crate::short_paths::guardian_short_id(guardian_id))
}

/// The internal ref a *pre-RAL-378* branch's review commits are built on.
///
/// Namespaced under `guardian/<id>/` so it could never collide with a user's
/// own branches. Readable naming trades that immunity for a name that can be
/// pushed as the PR branch directly (see [`crate::review_branch`]), so this is
/// only reached for branches registered before that landed.
fn legacy_branch_review_ref(guardian_id: &str, branch: &str) -> String {
    format!("guardian/{guardian_id}/wt-{branch}")
}

/// The internal combined-worktree ref for a pre-RAL-378 review.
fn legacy_combined_review_ref(guardian_id: &str) -> String {
    format!("guardian/{guardian_id}/review")
}

/// The ref one branch's review commits live on -- its claimed readable name
/// when it has one, the internal ref otherwise.
///
/// Read-only: unlike [`claim_branch_review_ref`] this never resolves or
/// persists a name, so a readable branch whose first build hasn't happened yet
/// still reports the internal ref. Every caller here is inspecting refs that
/// only exist *after* a build, so that fallback simply fails to resolve and the
/// caller takes its "no prior state" path.
pub(crate) fn review_ref_of(guardian_id: &str, branch: &crate::guardian::BranchView) -> String {
    named_review_ref(
        guardian_id,
        &branch.branch,
        branch.readable_review_branch,
        branch.review_branch_name.as_deref(),
    )
}

/// [`review_ref_of`] for the lighter [`crate::guardian::OrderedBranch`] the
/// merge loops iterate.
pub(crate) fn review_ref_of_ordered(
    guardian_id: &str,
    branch: &crate::guardian::OrderedBranch,
) -> String {
    named_review_ref(
        guardian_id,
        &branch.branch,
        branch.readable_review_branch,
        branch.review_branch_name.as_deref(),
    )
}

fn named_review_ref(
    guardian_id: &str,
    branch: &str,
    readable: bool,
    claimed: Option<&str>,
) -> String {
    match claimed {
        Some(name) if readable && !name.is_empty() => name.to_string(),
        _ => legacy_branch_review_ref(guardian_id, branch),
    }
}

/// The ref a review's combined worktree branch lives on, read-only.
pub(crate) fn combined_review_ref_of(guardian: &crate::guardian::GuardianView) -> String {
    match guardian.review_branch_name.as_deref() {
        Some(name) if guardian.readable_review_branch && !name.is_empty() => name.to_string(),
        _ => legacy_combined_review_ref(&guardian.id),
    }
}

/// [`claim_combined_review_ref`] for the merge paths that hold `id` rather
/// than an already-loaded [`crate::guardian::GuardianView`].
fn claim_combined_review_ref_by_id(
    store: &Arc<Mutex<Store>>,
    root: &Workspace,
    id: &str,
) -> std::result::Result<String, String> {
    let guardian = store
        .lock()
        .expect("poisoned")
        .get_guardian(id)
        .map_err(|e| e.to_string())?;
    claim_combined_review_ref(store, root, &guardian)
}

/// Every readable review-branch name this review has claimed, restricted to
/// `project` when one is given (a multi-project review's branches live in
/// different repos, and a name only exists as a ref in its own).
///
/// The combined-worktree name is included for the review's primary
/// `git_root` only -- there is exactly one combined branch, and it lives
/// there.
fn claimed_review_branches(
    store: &Arc<Mutex<Store>>,
    id: &str,
    project: Option<&str>,
) -> Vec<String> {
    let Ok(guardian) = store.lock().expect("poisoned").get_guardian(id) else {
        return Vec::new();
    };
    let mut names: Vec<String> = guardian
        .branches
        .iter()
        .filter(|b| {
            project.is_none_or(|p| b.project.as_deref().unwrap_or(guardian.git_root.as_str()) == p)
        })
        .filter_map(|b| b.review_branch_name.clone())
        .filter(|n| !n.is_empty())
        .collect();
    if project.is_none_or(|p| p == guardian.git_root) {
        names.extend(
            guardian
                .review_branch_name
                .clone()
                .filter(|n| !n.is_empty()),
        );
    }
    names.sort();
    names.dedup();
    names
}

/// Whether a local branch `name` already exists in `root`.
fn local_branch_exists(root: &Workspace, name: &str) -> bool {
    root.git(&[
        "show-ref",
        "--verify",
        "--quiet",
        &format!("refs/heads/{name}"),
    ])
    .is_ok()
}

/// Serializes the whole check-then-persist of a review-branch name.
///
/// Resolving a name reads live git refs and live store rows, so it cannot be
/// done under the store lock (subprocess waits never happen there) and is
/// therefore not atomic on its own: two merges building different reviews of
/// the same repo at the same time could both see `x-review` free and both
/// claim it, after which each `worktree_add_or_reset` would reset the other's
/// branch out from under it. Claims are rare and take milliseconds, so one
/// process-wide lock held across the resolve and the write is the whole fix.
static REVIEW_BRANCH_CLAIM_LOCK: Mutex<()> = Mutex::new(());

/// Resolve `base` to a name no local branch, claimed review-branch name or
/// open PR alias in `project_root` is using, ignoring the branch `except`
/// identifies (its own prior claim is not a collision with itself).
///
/// Callers must hold [`REVIEW_BRANCH_CLAIM_LOCK`] across this *and* the write
/// that records the result.
fn unique_review_branch_name(
    store: &Arc<Mutex<Store>>,
    root: &Workspace,
    project_root: &str,
    base: &str,
    except: Option<(&str, &str)>,
) -> std::result::Result<String, String> {
    crate::review_branch::resolve_unique(base, |candidate| {
        if local_branch_exists(root, candidate) {
            return true;
        }
        // A store error is treated as "taken" so a name is never claimed on
        // the strength of a failed lookup -- the loop then runs out of
        // attempts and the caller fails the branch loudly.
        store
            .lock()
            .expect("poisoned")
            .review_branch_name_taken(project_root, candidate, except)
            .unwrap_or(true)
    })
    .ok_or_else(|| {
        format!(
            "could not find a free review branch name for \"{base}\" after {} attempts",
            crate::review_branch::MAX_SUFFIX_ATTEMPTS
        )
    })
}

/// The ref this branch's review commits are built on, claiming and persisting
/// a readable name on first use (RAL-378).
///
/// Idempotent: once a name is persisted it is returned verbatim forever, which
/// is what keeps an already-open PR pointing at the branch it was opened from.
/// A pre-RAL-378 branch short-circuits to its internal ref and never claims
/// anything.
#[allow(clippy::too_many_arguments)]
fn claim_branch_review_ref(
    store: &Arc<Mutex<Store>>,
    root: &Workspace,
    guardian_id: &str,
    project_root: &str,
    branch_id: &str,
    branch: &str,
    readable: bool,
    claimed: Option<&str>,
) -> std::result::Result<String, String> {
    if !readable {
        return Ok(legacy_branch_review_ref(guardian_id, branch));
    }
    if let Some(name) = claimed.filter(|n| !n.is_empty()) {
        return Ok(name.to_string());
    }
    let base = crate::review_branch::base_from_task_branch(branch);
    let _claiming = REVIEW_BRANCH_CLAIM_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let name = unique_review_branch_name(
        store,
        root,
        project_root,
        &base,
        Some((guardian_id, branch_id)),
    )?;
    store
        .lock()
        .expect("poisoned")
        .set_branch_review_branch_name(guardian_id, branch_id, &name)
        .map_err(|e| e.to_string())?;
    crate::rlog!(
        INFO,
        "ralphus [guardian] review {guardian_id} branch {branch_id} review branch named {name}"
    );
    let _ = store.lock().expect("poisoned").log_event(
        None,
        Some(guardian_id),
        "guardian",
        None,
        "review branch named",
    );
    Ok(name)
}

/// [`claim_branch_review_ref`] for a combined-worktree review's single shared
/// branch, named from the review's own name rather than any one task branch.
fn claim_combined_review_ref(
    store: &Arc<Mutex<Store>>,
    root: &Workspace,
    guardian: &crate::guardian::GuardianView,
) -> std::result::Result<String, String> {
    if !guardian.readable_review_branch {
        return Ok(legacy_combined_review_ref(&guardian.id));
    }
    if let Some(name) = guardian
        .review_branch_name
        .as_deref()
        .filter(|n| !n.is_empty())
    {
        return Ok(name.to_string());
    }
    let base = crate::review_branch::base_from_review_name(&guardian.name);
    let _claiming = REVIEW_BRANCH_CLAIM_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let name = unique_review_branch_name(store, root, &guardian.git_root, &base, None)?;
    store
        .lock()
        .expect("poisoned")
        .set_guardian_review_branch_name(&guardian.id, &name)
        .map_err(|e| e.to_string())?;
    crate::rlog!(
        INFO,
        "ralphus [guardian] review {} combined review branch named {name}",
        guardian.id
    );
    let _ = store.lock().expect("poisoned").log_event(
        None,
        Some(&guardian.id),
        "guardian",
        None,
        "combined review branch named",
    );
    Ok(name)
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
    let claimed = claimed_review_branches(store, id, Some(git_root));
    cleanup_review_worktrees(&root, &wt_base, id, &num, &claimed);
    // Also drop any carry-forward protection refs so a deleted guardian leaves
    // nothing pinning otherwise-unreachable commits.
    purge_carry_refs(&root, id);
}

/// Validate the guardian and kick off a background merge. Returns immediately.
/// Kick off a background merge for `id`. The spawned worker acquires a slot
/// from `sem` before doing any work, so the review counts against the same
/// global concurrency cap as cells and task-level proofs.
///
/// RAL-213: registers a cancel token under `guardian:{id}` in `cancellations`
/// for the lifetime of the spawned merge (removed once it returns), mirroring
/// `scheduler::tick`'s register/remove wrapping for squads -- this is what lets
/// a settings change (or an explicit cancel-and-restart) actually stop this
/// merge instead of only flipping a DB column underneath it.
pub fn start_merge(
    store: Arc<Mutex<Store>>,
    runner: Arc<dyn Runner>,
    id: &str,
    sem: Arc<Semaphore>,
    cancellations: Cancellations,
) -> Reply {
    match kickoff_merge(store, runner, id, sem, cancellations) {
        Ok(StartMergeOutcome::Merging) => reply(202, "{\"status\":\"merging\"}"),
        Ok(StartMergeOutcome::Deferred) => reply(
            202,
            "{\"status\":\"deferred\",\"message\":\"merge deferred until every enabled branch is ready\"}",
        ),
        Ok(StartMergeOutcome::AlreadyInProgress) => reply(
            409,
            &error_body(
                "already_in_progress",
                "a rebase is already in progress; cancel it before starting a new one",
            ),
        ),
        Ok(StartMergeOutcome::AlreadyMerged) => reply(
            200,
            "{\"status\":\"approved\",\"message\":\"this review's work was already merged\"}",
        ),
        Err(StartMergeError::NotFound(message)) => reply(404, &error_body("not_found", &message)),
        Err(StartMergeError::NoBranches) => reply(
            400,
            &error_body("no_branches", "guardian has no branches to merge"),
        ),
        Err(StartMergeError::Store(message)) => reply(500, &error_body("store_error", &message)),
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum StartMergeError {
    NotFound(String),
    NoBranches,
    Store(String),
}

/// Take the global store lock, logging how long the wait took.
///
/// Every acquisition on the merge-kickoff path goes through here. Kicking off
/// a rebase is a DB state transition plus a thread spawn, so any wall time it
/// spends is almost entirely time queued behind another holder of this mutex;
/// naming the wait in the log is what turns "the button did nothing for
/// twenty seconds" into a locatable cause.
fn lock_timed<'a>(
    store: &'a Arc<Mutex<Store>>,
    id: &str,
    what: &str,
) -> std::sync::MutexGuard<'a, Store> {
    let waiting = std::time::Instant::now();
    let guard = store.lock().expect("store mutex poisoned");
    let waited_ms = waiting.elapsed().as_millis();
    if waited_ms >= KICKOFF_SLOW_LOCK_MS {
        // ralphus[ignore-rlog-pair]: internal lock-wait perf diagnostic, not a queryable domain event
        crate::rlog!(
            WARNING,
            "ralphus [guardian] review {id} merge kickoff waited {waited_ms}ms for the store lock ({what})"
        );
    } else {
        // ralphus[ignore-rlog-pair]: internal lock-wait perf diagnostic, not a queryable domain event
        crate::rlog!(
            DEBUG,
            "ralphus [guardian] review {id} merge kickoff store lock ({what}) after {waited_ms}ms"
        );
    }
    guard
}

/// Store-lock wait at or above which [`lock_timed`] escalates to `WARNING`.
/// A kickoff that waits this long is queued behind a holder doing real work
/// (a git subprocess, a forge call), which is the thing worth finding.
const KICKOFF_SLOW_LOCK_MS: u128 = 250;

pub(crate) fn kickoff_merge(
    store: Arc<Mutex<Store>>,
    runner: Arc<dyn Runner>,
    id: &str,
    sem: Arc<Semaphore>,
    cancellations: Cancellations,
) -> Result<StartMergeOutcome, StartMergeError> {
    let kickoff_started = std::time::Instant::now();
    // RAL-300: a manual "Merge / rebase" trigger must not waste a rebuild
    // when every linked PR has already merged -- ask first, exactly like the
    // periodic sweep (`review_maintenance`) does. When this settles the
    // review by approving it outright, report that instead of falling
    // through to the ordinary claim/rebuild path below (which would just
    // find nothing left to claim and 409). A mid-flight PR drop (guardian
    // status unchanged) falls straight through to the normal path.
    if crate::pr::check_pr_merges(&store, id) {
        let now_approved = matches!(
            store.lock().expect("store mutex poisoned").get_guardian(id),
            Ok(g) if g.status.as_str() == GuardianStatus::Approved.as_str()
        );
        if now_approved {
            return Ok(StartMergeOutcome::AlreadyMerged);
        }
    }
    // RAL-300: same idea, but via git ancestry rather than the forge -- a
    // review whose base branch already contains every enabled branch's
    // commits (e.g. a fast-forward merge outside any tracked PR) has
    // nothing left to rebuild either.
    let guardian_snapshot = {
        let guard = store.lock().expect("store mutex poisoned");
        guard.get_guardian(id).ok()
    };
    if let Some(guardian_snapshot) = guardian_snapshot {
        if guardian_snapshot.status.as_str() == GuardianStatus::InReview.as_str()
            && guardian_base_already_has_every_branch(&store, id, &guardian_snapshot)
            && approve_base_already_landed(&store, id)
        {
            return Ok(StartMergeOutcome::AlreadyMerged);
        }
    }
    // The guardian read and the "is any branch still waiting on its cell?"
    // check share one lock acquisition: they are two reads of the same
    // snapshot, and every extra acquisition is another chance to queue behind
    // a long-running holder.
    let (guardian, unfinished) = {
        let guard = lock_timed(&store, id, "read");
        let guardian = match guard.get_guardian(id) {
            Ok(g) => g,
            Err(e) => return Err(StartMergeError::NotFound(e.to_string())),
        };
        // Only a still-`collecting` guardian can have a branch genuinely
        // waiting on its upstream Cell -- see the check below for why.
        let unfinished = if guardian.status == GuardianStatus::Collecting.as_str() {
            match guard.guardian_unfinished_linked_branches(id) {
                Ok(b) => b,
                Err(e) => return Err(StartMergeError::Store(e.to_string())),
            }
        } else {
            Vec::new()
        };
        (guardian, unfinished)
    };
    if guardian.branches.is_empty() {
        return Err(StartMergeError::NoBranches);
    }
    // A non-`collecting` guardian never reports unfinished branches (see
    // above): once it has left `collecting` -- reached `in_review`/
    // `merge_failed`, or is already `merging` -- every enabled branch has
    // gone through a full merge pass at least once, and a re-trigger (the
    // "Merge / rebase" button, a settings-change restart, a base-branch shift
    // on an already-built review) is a deliberate re-run that must not
    // silently no-op just because some cell's `cells.state` still reads back
    // non-`done` (e.g. it was never wired to a real task, like a manually
    // added test branch, or the review outlived its squad).
    if !unfinished.is_empty() {
        crate::rlog!(
            INFO,
            "ralphus [guardian] review {id} merge deferred: pending branches remain"
        );
        {
            let guard = lock_timed(&store, id, "defer log");
            let _ = guard.cartographer_log(crate::cartographer::CartographerEntry {
                level: crate::logging::LogLevel::INFO,
                source: "guardian",
                message: "merge deferred: pending branches remain",
                scope: Some("guardian"),
                squad_id: None,
                guardian_id: Some(id),
                cell_id: None,
                task: None,
                log_path: None,
                payload: serde_json::json!({ "pending_branches": unfinished }),
                admin_only: false,
            });
        }
        return Ok(StartMergeOutcome::Deferred);
    }

    // Atomically transition collecting, merge_failed, or in_review → merging
    // (RAL-108: in_review is included so "Merge / rebase" forces a fresh rebase
    // even on an already-done review). Two concurrent requests can both pass
    // the guardian-exists check above, but only one can win this SQL UPDATE;
    // the other gets false and a 409.
    // Claim and its Cartographer row share one acquisition -- the row records
    // the outcome of the claim that just happened under this same guard, so
    // splitting them only adds a second chance to queue behind a slow holder.
    let claimed = {
        let guard = lock_timed(&store, id, "claim");
        let claimed = match guard.claim_guardian_merge(id) {
            Ok(c) => c,
            Err(e) => return Err(StartMergeError::Store(e.to_string())),
        };
        let entry = if claimed {
            crate::cartographer::CartographerEntry {
                level: crate::logging::LogLevel::INFO,
                source: "guardian",
                message: "merge starting",
                scope: Some("guardian"),
                squad_id: None,
                guardian_id: Some(id),
                cell_id: None,
                task: None,
                log_path: None,
                payload: serde_json::json!({"branches": guardian.branches.len()}),
                admin_only: false,
            }
        } else {
            crate::cartographer::CartographerEntry {
                level: crate::logging::LogLevel::DEBUG,
                source: "guardian",
                message: "merge claim rejected (already merging, or in a terminal state)",
                scope: Some("guardian"),
                squad_id: None,
                guardian_id: Some(id),
                cell_id: None,
                task: None,
                log_path: None,
                payload: serde_json::json!({}),
                admin_only: false,
            }
        };
        let _ = guard.cartographer_log(entry);
        claimed
    };
    if !claimed {
        crate::rlog!(
            DEBUG,
            "ralphus [guardian] review {id} merge claim rejected (already merging, or in a terminal state) in {}ms",
            kickoff_started.elapsed().as_millis()
        );
        return Ok(StartMergeOutcome::AlreadyInProgress);
    }
    crate::rlog!(
        INFO,
        "ralphus [guardian] review {id} merge starting branches={} (kickoff {}ms)",
        guardian.branches.len(),
        kickoff_started.elapsed().as_millis()
    );
    let sid = id.to_string();
    std::thread::spawn(move || {
        let _permit = sem.acquire();
        let token = cancellations.register(&format!("guardian:{sid}"));
        run_merge_cancellable(&store, runner.as_ref(), &sid, &token);
        cancellations.remove(&format!("guardian:{sid}"));
    });
    Ok(StartMergeOutcome::Merging)
}

/// Bounded wait for a merge worker registered under `guardian:{id}` to
/// actually exit, mirroring `server::wait_for_worker_stop`'s doc comment
/// (same double-dispatch race, same 5s/50ms budget) for the guardian-merge
/// key namespace -- duplicated here rather than reused because that helper
/// takes a `&Daemon`, which this module has no handle to (only the
/// individual `store`/`cancellations`/`runner`/`sem` handles it needs).
fn wait_for_merge_worker_stop(cancellations: &Cancellations, key: &str) {
    const TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);
    const POLL_INTERVAL: std::time::Duration = std::time::Duration::from_millis(50);
    let started = std::time::Instant::now();
    while cancellations.is_active(key) {
        if started.elapsed() >= TIMEOUT {
            break;
        }
        std::thread::sleep(POLL_INTERVAL);
    }
}

/// Terminate every tmux-backed agent session belonging to this review.
///
/// The cancellation token remains the ordinary cooperative stop path. This is
/// the immediate path for an agent already inside a blocking invocation: on
/// Windows, [`crate::tmux::Tmux::kill_session`] also closes the confined job
/// object, terminating the pane's entire process tree rather than merely
/// removing the tmux session name.
fn kill_guardian_agent_sessions(store: &Arc<Mutex<Store>>, id: &str) {
    let prefix = format!("ralphus_guardian-{id}_");
    let count = crate::tmux::Tmux::resolve()
        .map(|tmux| tmux.kill_sessions_with_prefix(&prefix))
        .unwrap_or(0);
    if count == 0 {
        return;
    }
    crate::rlog!(
        INFO,
        "ralphus [guardian] review {id} stopped {count} active agent session(s)"
    );
    let guard = store.lock().expect("store mutex poisoned");
    let _ = guard.cartographer_log(crate::cartographer::CartographerEntry {
        level: crate::logging::LogLevel::INFO,
        source: "guardian",
        message: "stopped active review agent sessions",
        scope: Some("guardian"),
        squad_id: None,
        guardian_id: Some(id),
        cell_id: None,
        task: None,
        log_path: None,
        payload: serde_json::json!({ "session_count": count }),
        admin_only: false,
    });
}

/// Stop any in-flight merge worker for `id` (cancel token + the same bounded
/// wait as [`restart_guardian_merge`]/[`stop_guardian_merge`]) before a plain
/// cancel writes `cancelled` to the DB.
///
/// Without this, `Store::cancel_guardian` only flips the DB column: a live
/// merge worker's `cancel: &CancelToken` is never tripped, so it never kills
/// its resolver agent's tmux session, keeps running every remaining branch to
/// completion, and its own end-of-pass `set_guardian_status(InReview, ...)`
/// (unconditional -- there's no `WHERE status='cancelled'` guard) overwrites
/// the `cancelled` status straight back to `in_review`. Call this first so
/// the worker has already exited (or been given its best bounded chance to)
/// by the time the DB write happens, mirroring why `restart_guardian_merge`/
/// `stop_guardian_merge` both cancel-and-wait before touching guardian state.
pub fn stop_merge_worker_for_cancel(cancellations: &Cancellations, id: &str) {
    let key = format!("guardian:{id}");
    cancellations.cancel(&key);
    wait_for_merge_worker_stop(cancellations, &key);
}

/// Stop an in-flight merge for `id` (if any) and start a fresh one, safely.
///
/// RAL-213: this is the safe replacement for the old "reset to collecting,
/// then start a new merge thread without stopping the old one" pattern that
/// both `guardian_cancel_and_merge` and a guardian-settings change while
/// `merging` used to follow -- two `run_merge` calls for the same guardian id
/// operate on identical worktree paths/branch names/carry refs, so without
/// this the newer thread's cleanup could tear down worktrees/branches the
/// older thread was still rebasing in. Mirrors `server::restart_run`'s
/// cancel → wait → reset → restart shape exactly.
pub fn restart_guardian_merge(
    store: Arc<Mutex<Store>>,
    cancellations: Cancellations,
    runner: Arc<dyn Runner>,
    id: &str,
    sem: Arc<Semaphore>,
) -> Reply {
    let key = format!("guardian:{id}");
    cancellations.cancel(&key);
    kill_guardian_agent_sessions(&store, id);
    wait_for_merge_worker_stop(&cancellations, &key);
    if let Err(e) = store
        .lock()
        .expect("store mutex poisoned")
        .reset_guardian_to_collecting(id)
    {
        return reply(500, &error_body("store_error", &e.to_string()));
    }
    start_merge(store, runner, id, sem, cancellations)
}

/// Reopen a `cancelled` review (status → `collecting`) and immediately try an
/// incremental staged merge (RAL-265, [`run_merge_staged`]) -- the same pass
/// a task completion would have triggered via `start_reviews`
/// (`daemon/src/scheduler.rs`) had this review not been cancelled at the
/// time. Deliberately *not* the all-or-nothing [`start_merge`] that the
/// manual "Merge / rebase" button uses: that path waits for every enabled
/// branch's cell to finish before rebasing anything, so a review reopened
/// while one branch is still pending would sit doing nothing until that last
/// cell completes, even though every earlier branch's cell finished (and
/// would already have been rebased into the review) while the review was
/// dormant. Staging the ready prefix now catches it up immediately instead
/// of waiting on that last cell or the periodic maintenance sweep.
///
/// There is no live worker to cancel-and-wait for first, unlike
/// [`restart_guardian_merge`]: a cancelled review's merge worker already
/// exited before the `cancelled` status was written (see
/// `stop_merge_worker_for_cancel`). `claim_guardian_merge` still gates the
/// `collecting` → `merging` transition, so a concurrent trigger (another
/// reopen call, a task completing at the same moment) can't double-run the
/// staged pass.
pub fn reopen_cancelled_guardian_merge(
    store: Arc<Mutex<Store>>,
    runner: Arc<dyn Runner>,
    id: &str,
    sem: Arc<Semaphore>,
    cancellations: Cancellations,
) -> Reply {
    if let Err(e) = store
        .lock()
        .expect("store mutex poisoned")
        .reopen_cancelled_guardian(id)
    {
        return reply(500, &error_body("store_error", &e.to_string()));
    }
    let claimed = store
        .lock()
        .expect("store mutex poisoned")
        .claim_guardian_merge(id)
        .unwrap_or(false);
    if !claimed {
        // Lost the claim to a concurrent trigger (e.g. a task completing at
        // the same instant) -- that other caller's pass covers this reopen.
        return reply(202, "{\"status\":\"merging\"}");
    }
    let sid = id.to_string();
    std::thread::spawn(move || {
        let _permit = sem.acquire();
        let token = cancellations.register(&format!("guardian:{sid}"));
        run_merge_staged(&store, runner.as_ref(), &sid, &token);
        cancellations.remove(&format!("guardian:{sid}"));
    });
    reply(202, "{\"status\":\"merging\"}")
}

/// Halt an in-flight merge for `id` at its next checkpoint, leaving the review
/// in the recoverable `merge_stopped` state (RAL-249) rather than cancelled.
///
/// Distinct from [`restart_guardian_merge`]: it stops the live worker the same
/// way (cancel token + bounded wait) but then leaves the review and branches
/// alone instead of resetting to `collecting` and starting fresh — the next
/// `start_merge` (the board's "Merge / rebase" on a `merge_stopped` review)
/// resumes from the first review worktree, and the next merge's own setup phase
/// handles whatever leftovers this halt left behind. The store write is atomic
/// on `status='merging'`, so a merge that actually completed before the wait
/// gave up is left in `in_review` rather than mis-labelled.
pub fn stop_guardian_merge(
    store: Arc<Mutex<Store>>,
    cancellations: Cancellations,
    id: &str,
) -> Reply {
    let key = format!("guardian:{id}");
    cancellations.cancel(&key);
    kill_guardian_agent_sessions(&store, id);
    wait_for_merge_worker_stop(&cancellations, &key);
    match store
        .lock()
        .expect("store mutex poisoned")
        .stop_guardian_merge(id)
    {
        Ok(status) => {
            // ralphus[ignore-rlog-pair]: this low-level helper has no Store; its Store-owning caller records the structured workflow outcome
            crate::rlog!(
                INFO,
                "ralphus [guardian] review {id} merge stopped → {}",
                status.as_str()
            );
            reply(200, &format!("{{\"status\":\"{}\"}}", status.as_str()))
        }
        Err(e) => reply(500, &error_body("store_error", &e.to_string())),
    }
}

/// Kick off a background "set it for me" resolution of one named
/// [`CheckInput`] (RAL-164). Looks the input up across the guardian's
/// `manual_commands`/`action_hints` (first match wins) to recover the
/// command it's used in and its declared message/default, atomically claims
/// it via [`Store::claim_guardian_input_resolution`] so a concurrent
/// duplicate request 409s instead of spawning a second LLM call, then
/// spawns a background worker (gated by `sem`, same global concurrency cap
/// as merges/cells) and returns `202` immediately.
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
///
/// RAL-272: also persists the feedback text into that branch's read-only
/// feedback thread (`guardian_messages`, scoped by `branch_id`) and, in the
/// background, generates a short triage-style acknowledgment reply the same
/// way the old global chat did -- see [`record_feedback_reply`].
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
    let feature = match guardian.branches.iter().find(|b| b.id == branch_id) {
        Some(b) if b.worktree.is_some() => b.branch.clone(),
        Some(_) => {
            return reply(
                409,
                &error_body("not_ready", "run the review merge before giving feedback"),
            );
        }
        None => return reply(404, &error_body("not_found", "no such branch")),
    };
    if let Err(e) = store.lock().expect("poisoned").add_guardian_message(
        id,
        "reviewer",
        &feedback,
        None,
        Some(branch_id),
    ) {
        return reply(500, &error_body("internal", &e.to_string()));
    }
    let sid = id.to_string();
    let bid = branch_id.to_string();
    std::thread::spawn(move || {
        record_feedback_reply(&store, &sid, &bid, &feature, &feedback);
        let outcome = run_feedback(
            &store,
            runner.as_ref(),
            &sid,
            &bid,
            &feedback,
            &CancelToken::never(),
        );
        crate::rlog!(
            INFO,
            "ralphus [guardian] review {sid} feedback outcome committed={} pushed={}",
            outcome.committed,
            outcome.pushed
        );
    });
    reply(202, "{\"status\":\"applying_feedback\"}")
}

/// Generate a short, conversational acknowledgment of branch feedback and
/// persist it into that branch's read-only feedback thread (RAL-272), reusing
/// the same direct-LLM-call plumbing the old global chat used
/// (`chat_client::call_direct`) rather than the file-editing resolver agent
/// `run_feedback` spawns separately. Best-effort: any failure here is logged
/// and swallowed rather than failing the feedback-application flow.
fn record_feedback_reply(
    store: &Arc<Mutex<Store>>,
    id: &str,
    branch_id: &str,
    feature: &str,
    feedback: &str,
) {
    let guardian = match store.lock().expect("poisoned").get_guardian(id) {
        Ok(g) => g,
        Err(_) => return,
    };
    let resolved = match resolve_resolver_agent(
        guardian.resolver_agent.as_deref(),
        guardian.resolver_model.as_deref(),
        Path::new(&guardian.git_root),
    ) {
        Ok(r) => r,
        Err(e) => {
            // ralphus[ignore-rlog-pair]: this low-level helper has no Store; its Store-owning caller records the structured workflow outcome
            crate::rlog!(
                WARNING,
                "ralphus [guardian] review {id} branch {branch_id} feedback reply skipped: \
                 unresolvable resolver agent: {e}"
            );
            return;
        }
    };
    if resolved.custom_profile {
        // `call_direct` reads provider credentials straight from the daemon's
        // own environment and has no way to honor a custom profile's env
        // (e.g. an OpenRouter base URL/key), so skip rather than silently
        // hit the wrong endpoint/key.
        return;
    }
    let system = format!(
        "You are the review Guardian. A reviewer just left feedback on branch \
         '{feature}', which an agent is now applying in its review worktree. \
         Reply with a brief, conversational 1-2 sentence acknowledgment of what \
         you understood from the feedback. Do not describe git commands or ask \
         the reviewer to run anything themselves."
    );
    let messages = [crate::chat_client::ChatMessage {
        role: "user",
        content: feedback.to_string(),
        image: None,
    }];
    let reply_text = match crate::chat_client::call_direct(
        &resolved.backend,
        resolved.model.as_deref(),
        &system,
        &messages,
    ) {
        Ok(text) => text,
        Err(e) => {
            // ralphus[ignore-rlog-pair]: this low-level helper has no Store; its Store-owning caller records the structured workflow outcome
            crate::rlog!(
                WARNING,
                "ralphus [guardian] review {id} branch {branch_id} feedback reply skipped: {e}"
            );
            return;
        }
    };
    let guard = store.lock().expect("poisoned");
    let _ = guard.add_guardian_message(id, "guardian", &reply_text, None, Some(branch_id));
}

/// [`run_merge`] with no way to stop early -- for tests and any caller with no
/// live [`CancelToken`] to hand it (`CancelToken::never()` never trips).
pub fn run_merge(store: &Arc<Mutex<Store>>, runner: &dyn Runner, id: &str) {
    run_merge_cancellable(store, runner, id, &CancelToken::never());
}

/// What a single [`staged_merge_pass`] returned, and whether the caller should
/// keep looping.
enum StagedPassOutcome {
    /// The pass ran to completion. `built_any` is true iff it rebased at least
    /// one branch this pass.
    Ok { built_any: bool },
    /// A branch failed; `fail_branch` already set the branch + guardian to
    /// `failed`/`merge_failed` with a detail message, so there's nothing more
    /// to record.
    Failed,
    /// The pass was cancelled at a checkpoint (`log_merge_cancelled` already
    /// ran); leave the merge state for the next pass's cleanup.
    Cancelled,
}

/// RAL-265: incremental stack rebase. Unlike the all-or-nothing
/// [`run_merge_cancellable`], which waits for every enabled branch's
/// contributing cell to finish before rebasing anything and then rebuilds the
/// whole stack from the base in one synchronous pass, this builds the stack
/// piecewise:
///
/// - Each *pass* rebases the contiguous prefix of branches whose cells are
///   done, stopping at the first branch still waiting on its upstream task.
/// - A still-valid `Done`/`ConflictResolved` prefix (same branch config and
///   same per-project base — see [`staged_build_signature`]) is preserved and
///   the next pass seeds its `prev_ref` from that prefix's last tip rather
///   than rebuilding it from the base. A changed base or branch config changes
///   the signature, so the next pass rebuilds the prefix from the base instead
///   of trusting a stale `Done` tip.
/// - Only when *every* enabled branch across *every* project is done does it
///   finalize: combined worktrees, manual command generation, final check
///   gates, and the `InReview` transition (mirroring `run_merge_cancellable`'s
///   tail). Otherwise it returns to `Collecting` so a later task completion
///   re-triggers a further pass via `try_start_ready_reviews_for_task`.
/// - The residual race — a branch becoming `ready` *while* a pass is already
///   building, so `try_start_ready_reviews_for_task`'s claim loses to the
///   running merge — is closed by the pass-level loop below coalescing
///   consecutive completions instead of dropping them.
///
/// `claim_guardian_merge` (collecting→merging) already guarantees a single
/// merge runs at a time, so concurrent task completions can never double-trigger
/// a pass.
///
/// `skip_worktrees` reviews (shared worktree, no per-branch worktrees) are not
/// incremental in this design: they fall back to the legacy all-or-nothing
/// `run_merge_cancellable`, once every enabled branch's cells are done.
pub fn run_merge_staged(
    store: &Arc<Mutex<Store>>,
    runner: &dyn Runner,
    id: &str,
    cancel: &CancelToken,
) {
    let guardian = match store.lock().expect("poisoned").get_guardian(id) {
        Ok(g) => g,
        Err(_) => return,
    };
    if guardian.skip_worktrees {
        // Shared-worktree reviews have no per-branch worktrees to resume from,
        // so the staged skip-and-resume has nothing to preserve. Reuse the
        // legacy merge, but only once every enabled branch's cells are done
        // (the only input it can build); otherwise stay collecting for now.
        if !all_enabled_branches_terminal(store, id) {
            let _ = store.lock().expect("poisoned").set_guardian_status(
                id,
                GuardianStatus::Collecting,
                None,
            );
        } else {
            run_merge_cancellable(store, runner, id, cancel);
        }
        return;
    }
    let set_status = |s: GuardianStatus, detail: Option<&str>| {
        let _ = store
            .lock()
            .expect("poisoned")
            .set_guardian_status(id, s, detail);
    };
    let resolved = match resolve_resolver_agent(
        guardian.resolver_agent.as_deref(),
        guardian.resolver_model.as_deref(),
        Path::new(&guardian.git_root),
    ) {
        Ok(resolved) => resolved,
        Err(error) => {
            set_status(
                GuardianStatus::MergeFailed,
                Some(&format!("unresolvable resolver agent: {error}")),
            );
            return;
        }
    };
    if let Err(error) = preflight_resolver_agent(runner, &resolved, guardian.machine.as_deref()) {
        set_status(GuardianStatus::MergeFailed, Some(&error));
        return;
    }
    set_status(GuardianStatus::Merging, None);
    {
        let guard = store.lock().expect("poisoned");
        // RAL-103/RAL-27: clear the previous build's manual-check commands and
        // conflict bookkeeping up front so the board never shows a stale
        // "checks ready" state or leftover conflict progress while the staged
        // build is in flight. (Manual commands are only regenerated on final
        // InReview, not on a partial pass.)
        let _ = guard.clear_guardian_manual_commands(id);
        let _ = guard.set_guardian_conflicts(id, None, None, None);
        let _ = guard.clear_all_branch_conflicts(id);
    }
    crate::rlog!(
        INFO,
        "ralphus [guardian] review {id} staged merge executing (incremental stack rebase)"
    );
    {
        let guard = store.lock().expect("poisoned");
        let _ = guard.cartographer_log(crate::cartographer::CartographerEntry {
            level: crate::logging::LogLevel::INFO,
            source: "guardian",
            message: "staged merge executing",
            scope: Some("guardian"),
            squad_id: None,
            guardian_id: Some(id),
            cell_id: None,
            task: None,
            log_path: None,
            payload: serde_json::json!({}),
            admin_only: false,
        });
    }

    let mut built_any_total = false;
    loop {
        if cancel.is_cancelled() {
            log_merge_cancelled(store, id);
            return;
        }
        match staged_merge_pass(store, runner, id, cancel) {
            StagedPassOutcome::Cancelled => return,
            StagedPassOutcome::Failed => return,
            StagedPassOutcome::Ok { built_any } => {
                if built_any {
                    built_any_total = true;
                }
                if all_enabled_branches_terminal(store, id) {
                    // Whole stack is rebased — finalize now, exactly once.
                    if built_any_total {
                        let _ = store
                            .lock()
                            .expect("poisoned")
                            .bump_guardian_merge_attempt(id);
                    }
                    finish_staged_merge(store, runner, id, cancel, &set_status);
                    return;
                }
                if !built_any || !next_not_built_is_ready(store, id) {
                    // Nothing more can build right now: return to Collecting so
                    // the next task completion re-triggers a further pass.
                    set_status(GuardianStatus::Collecting, None);
                    return;
                }
                // A `pending` branch became `ready` while we were building;
                // coalesce it into this same merge rather than waiting.
            }
        }
    }
}

/// Every enabled branch across every project of `id` is in a terminal
/// (rebase-complete) state — `done` or `conflict_resolved`. This is the gate
/// for the `InReview` finalize and for the shared-worktree fallback.
fn all_enabled_branches_terminal(store: &Arc<Mutex<Store>>, id: &str) -> bool {
    let Ok(g) = store.lock().expect("poisoned").get_guardian(id) else {
        return false;
    };
    g.branches.iter().filter(|b| b.enabled).all(|b| {
        b.merge_status == MergeStatus::Done.as_str()
            || b.merge_status == MergeStatus::ConflictResolved.as_str()
    })
}

/// Whether the first enabled branch that is NOT yet rebase-complete (`done` /
/// `conflict_resolved`) is in the `ready` state — i.e. another staged pass
/// would build at least one branch immediately. Used to coalesce a branch that
/// became `ready` mid-pass.
fn next_not_built_is_ready(store: &Arc<Mutex<Store>>, id: &str) -> bool {
    let Ok(g) = store.lock().expect("poisoned").get_guardian(id) else {
        return false;
    };
    let mut branches: Vec<_> = g.branches.iter().filter(|b| b.enabled).collect();
    branches.sort_by_key(|b| b.position);
    match branches.into_iter().find(|b| {
        b.merge_status != MergeStatus::Done.as_str()
            && b.merge_status != MergeStatus::ConflictResolved.as_str()
    }) {
        Some(b) => b.merge_status == MergeStatus::Ready.as_str(),
        None => false,
    }
}

/// Hash identifying the stack configuration a staged pass builds against: each
/// project's resolved base commit plus the ordered identity of every enabled
/// branch (position, feature branch name, owning project). Two passes share a
/// signature iff the base is unchanged AND the branch set is unchanged, which
/// is exactly when a previously-`Done` prefix is still valid to resume from.
///
/// `base_shas` is `{project_root: resolved_base_sha}` resolved by the caller.
fn staged_build_signature(
    project_order: &[String],
    base_shas: &std::collections::HashMap<String, String>,
    enabled_branches: &[crate::guardian::BranchView],
) -> String {
    let mut parts: Vec<String> = Vec::new();
    for proj in project_order {
        if let Some(sha) = base_shas.get(proj) {
            parts.push(format!("base:{proj}:{sha}"));
        }
    }
    let branch_signature = staged_branch_signature(enabled_branches);
    if !branch_signature.is_empty() {
        parts.push(branch_signature);
    }
    parts.join("|")
}

/// The branch-config suffix of [`staged_build_signature`], kept separate so a
/// pass can distinguish a base-only change from a reordered/added/removed
/// branch set. Only a base-only rebuild may replay prior resolved review tips.
fn staged_branch_signature(enabled_branches: &[crate::guardian::BranchView]) -> String {
    let mut ordered: Vec<&crate::guardian::BranchView> =
        enabled_branches.iter().filter(|b| b.enabled).collect();
    ordered.sort_by_key(|b| b.position);
    ordered
        .into_iter()
        .map(|b| {
            let proj = b.project.clone().unwrap_or_default();
            format!("branch:{proj}:{}:{}", b.position, b.branch)
        })
        .collect::<Vec<_>>()
        .join("|")
}

fn signature_has_branch_config(signature: &str, branch_signature: &str) -> bool {
    signature == branch_signature
        || signature
            .strip_suffix(branch_signature)
            .is_some_and(|prefix| prefix.ends_with('|'))
}

/// One incremental pass: rebase the contiguous ready prefix of each project,
/// reusing a still-valid `Done` prefix when the recorded signature matches the
/// current one, rebuilding it (from the base) when it doesn't. Stops at the
/// first `pending` (cells-not-done) branch per project. Records the fresh
/// signature so the next pass can resume.
#[allow(clippy::too_many_arguments)]
fn staged_merge_pass(
    store: &Arc<Mutex<Store>>,
    runner: &dyn Runner,
    id: &str,
    cancel: &CancelToken,
) -> StagedPassOutcome {
    let guardian = match store.lock().expect("poisoned").get_guardian(id) {
        Ok(g) => g,
        Err(_) => return StagedPassOutcome::Ok { built_any: false },
    };
    let git_root = guardian.git_root.clone();

    // Partition enabled branches by project, preserving position order within
    // each project and project-first-appearance across the guardian.
    let mut project_order: Vec<String> = Vec::new();
    let mut project_branches: std::collections::HashMap<String, Vec<crate::guardian::BranchView>> =
        std::collections::HashMap::new();
    for b in guardian.branches.iter().filter(|b| b.enabled) {
        let proj = b.project.clone().unwrap_or_else(|| git_root.clone());
        if !project_branches.contains_key(&proj) {
            project_order.push(proj.clone());
        }
        project_branches.entry(proj).or_default().push(b.clone());
    }
    for branches in project_branches.values_mut() {
        branches.sort_by_key(|b| b.position);
    }

    let set_status = |s: GuardianStatus, d: Option<&str>| {
        let _ = store
            .lock()
            .expect("poisoned")
            .set_guardian_status(id, s, d);
    };

    // Resolve every project's base commit up front; if any base is unresolvable
    // the whole pass fails (mirrors run_merge_cancellable).
    let mut base_shas: std::collections::HashMap<String, String> = std::collections::HashMap::new();
    for proj in &project_order {
        let root = Workspace::for_guardian(store, id, PathBuf::from(&proj));
        match resolve_base(&root, &guardian.base_branch) {
            Ok(sha) => {
                base_shas.insert(proj.clone(), sha.clone());
                let _ = store
                    .lock()
                    .expect("poisoned")
                    .set_guardian_project_base_commit(id, proj, &sha);
            }
            Err(e) => {
                set_status(
                    GuardianStatus::MergeFailed,
                    Some(&format!(
                        "{proj}: base branch '{}': {e}",
                        guardian.base_branch
                    )),
                );
                return StagedPassOutcome::Failed;
            }
        }
    }

    let enabled_branches: Vec<crate::guardian::BranchView> = guardian
        .branches
        .iter()
        .filter(|b| b.enabled)
        .cloned()
        .collect();
    let current_sig = staged_build_signature(&project_order, &base_shas, &enabled_branches);
    let branch_sig = staged_branch_signature(&enabled_branches);
    let stored_sig = store
        .lock()
        .expect("poisoned")
        .guardian_build_signature(id)
        .unwrap_or(None);
    // Resume only when the recorded config/base matches the current one; a
    // mismatch (base moved, branch reordered/added/removed/enabled/disabled)
    // forces a rebuild of the previously-`Done` prefix so a stale tip is never
    // treated as valid.
    let resume = stored_sig.as_deref() == Some(current_sig.as_str());
    let base_only_rebuild = !resume
        && !branch_sig.is_empty()
        && stored_sig
            .as_deref()
            .is_some_and(|sig| signature_has_branch_config(sig, &branch_sig));

    // Unambiguous last branch in the stack for `ProofScope::FinalBranch`.
    let final_id = final_branch_id(
        &enabled_branches
            .iter()
            .map(|b| crate::guardian::OrderedBranch {
                id: b.id.clone(),
                position: b.position,
                branch: b.branch.clone(),
                enabled: b.enabled,
                readable_review_branch: b.readable_review_branch,
                review_branch_name: b.review_branch_name.clone(),
            })
            .collect::<Vec<_>>(),
    )
    .map(str::to_string);

    let mut built_any = false;
    for proj in &project_order {
        if cancel.is_cancelled() {
            return StagedPassOutcome::Cancelled;
        }
        let root = Workspace::for_guardian(store, id, PathBuf::from(&proj));
        let wt_base = root.at(worktree_dir(proj, id));
        let base_sha = base_shas.get(proj).cloned().unwrap_or_default();
        if let Err(e) = preflight_worktree_budget(&root, &wt_base, &base_sha) {
            set_status(GuardianStatus::MergeFailed, Some(&e));
            return StagedPassOutcome::Failed;
        }
        // For resume, reuse the maximal `Done` prefix's last tip as the seed;
        // otherwise seed from the base (full rebuild of the ready prefix).
        let proj_branches = &project_branches[proj];
        let (mut prev_ref, start_idx) = if resume {
            staged_resume_point(&root, id, proj_branches, &base_sha)
        } else {
            (base_sha.clone(), 0usize)
        };
        // A base-only signature change invalidates the prefix, but the existing
        // review refs still contain any conflict resolutions from the prior
        // build. Validate the complete old stack chain before using any of it;
        // branch-config changes deliberately rebuild from feature tips instead.
        let carry_chain = if base_only_rebuild {
            let old_base = guardian.base_commits.get(proj).cloned().or_else(|| {
                (proj == &guardian.git_root)
                    .then(|| guardian.base_commit.clone())
                    .flatten()
            });
            old_base.and_then(|mut old_upstream| {
                let mut chain = Vec::with_capacity(proj_branches.len());
                for bv in proj_branches {
                    let rev = review_ref_of(id, bv);
                    let old_tip = root.git(&["rev-parse", "--verify", &rev]).ok()?;
                    let old_tip = old_tip.trim().to_string();
                    if !is_ancestor(&root, &old_upstream, &old_tip) {
                        return None;
                    }
                    chain.push((old_tip.clone(), old_upstream));
                    old_upstream = old_tip;
                }
                Some(chain)
            })
        } else {
            None
        };
        let short_names = branch_short_names(store, id, Some(proj.as_str()));
        let squash = guardian.squash_projects.iter().any(|p| p == proj);
        for (idx, bv) in proj_branches.iter().enumerate() {
            if idx < start_idx {
                continue; // preserved, already-built prefix
            }
            if cancel.is_cancelled() {
                return StagedPassOutcome::Cancelled;
            }
            // Stop the project's build at the first branch whose cells aren't
            // all done yet.
            if bv.merge_status == MergeStatus::Pending.as_str() {
                break;
            }
            let _ = store.lock().expect("poisoned").set_branch_status(
                id,
                &bv.id,
                MergeStatus::InProgress,
                None,
            );
            if let Err(e) = fetch_branch_for_remote_cell(store, id, bv) {
                fail_branch(store, id, &bv.id, &bv.branch, &e, &set_status);
                return StagedPassOutcome::Failed;
            }
            let rev = match claim_branch_review_ref(
                store,
                &root,
                id,
                proj,
                &bv.id,
                &bv.branch,
                bv.readable_review_branch,
                bv.review_branch_name.as_deref(),
            ) {
                Ok(rev) => rev,
                Err(e) => {
                    fail_branch(store, id, &bv.id, &bv.branch, &e, &set_status);
                    return StagedPassOutcome::Failed;
                }
            };
            let wt = branch_wt_dir(&wt_base, &short_names, &bv.branch);
            let wt_str = wt.root().to_string_lossy().to_string();
            let (source_ref, upstream) = carry_chain
                .as_ref()
                .and_then(|chain| chain.get(idx))
                .map_or_else(
                    || (bv.branch.as_str(), base_sha.as_str()),
                    |(old_tip, old_upstream)| (old_tip.as_str(), old_upstream.as_str()),
                );
            if let Err(e) = worktree_add_or_reset(&root, &rev, &wt, source_ref) {
                fail_branch(store, id, &bv.id, &bv.branch, &e, &set_status);
                return StagedPassOutcome::Failed;
            }
            let _ = store
                .lock()
                .expect("poisoned")
                .set_branch_review(id, &bv.id, &rev, &wt_str);
            let gate = ProofGate::resolve(store, id, Some(&bv.id) == final_id.as_ref());
            if cancel.is_cancelled() {
                return StagedPassOutcome::Cancelled;
            }
            // `upstream` is the rebase boundary; `prev_ref` is what the branch
            // stacks onto (base at the first build of a project, else the prior
            // branch's tip — preserved on resume, the last rebuilt branch on a
            // rebuild).
            if stack_pick(
                store, runner, id, &bv.id, &bv.branch, upstream, &prev_ref, &rev, &wt, squash,
                &gate, cancel,
            )
            .is_err()
            {
                // stack_pick already set this branch + guardian failed.
                return StagedPassOutcome::Failed;
            }
            if cancel.is_cancelled() {
                return StagedPassOutcome::Cancelled;
            }
            if let Err(e) = run_commit_checks(store, id, &bv.id, &wt, &bv.branch, cancel) {
                fail_branch(store, id, &bv.id, &bv.branch, &e, &set_status);
                return StagedPassOutcome::Failed;
            }
            if note_if_branch_is_empty(store, id, &bv.id, &bv.branch, &root, upstream) {
                fail_branch(
                    store,
                    id,
                    &bv.id,
                    &bv.branch,
                    "branch is empty: it adds no changes over the branch beneath it in the stack.                      Its task most likely never committed its work -- check that cell, then re-run                      it. If this branch is meant to be empty, disable it to drop it from the stack.",
                    &set_status,
                );
                return StagedPassOutcome::Failed;
            }
            prev_ref = rev;
            built_any = true;
        }
    }

    // Record the current config/base so the next pass recognizes this prefix as
    // valid to resume from (it IS valid — it was just built against this
    // signature, and it is the longest built prefix).
    let _ = store
        .lock()
        .expect("poisoned")
        .set_guardian_build_signature(id, &current_sig);

    StagedPassOutcome::Ok { built_any }
}

/// For one project's ordered branches, find the maximal contiguous prefix that
/// is already rebase-complete (`done`/`conflict_resolved`) AND whose review ref
/// still resolves. Returns `(seed_prev_ref, first_index_to_build)`. When
/// nothing can be preserved, seeds from `base_sha` and starts at index 0.
fn staged_resume_point(
    root: &Workspace,
    id: &str,
    proj_branches: &[crate::guardian::BranchView],
    base_sha: &str,
) -> (String, usize) {
    let mut prev_ref = base_sha.to_string();
    let mut start = 0usize;
    for (i, bv) in proj_branches.iter().enumerate() {
        let terminal = bv.merge_status == MergeStatus::Done.as_str()
            || bv.merge_status == MergeStatus::ConflictResolved.as_str();
        if !terminal {
            break;
        }
        let rev = review_ref_of(id, bv);
        if root.git(&["rev-parse", "--verify", &rev]).is_err() {
            // The tip ref is gone (e.g. a worktree prune removed it) — the
            // prefix is no longer reusable; rebuild everything from here.
            break;
        }
        prev_ref = rev;
        start = i + 1;
    }
    (prev_ref, start)
}

/// Finalize a fully-rebased staged merge: build each project's combined
/// worktree, regenerate manual commands, run the final check gates, snapshot
/// review heads for manual-push detection, and move to `InReview`. Mirrors the
/// tail of [`run_merge_cancellable`].
#[allow(clippy::too_many_arguments)]
fn finish_staged_merge<F: Fn(GuardianStatus, Option<&str>)>(
    store: &Arc<Mutex<Store>>,
    runner: &dyn Runner,
    id: &str,
    cancel: &CancelToken,
    set_status: &F,
) {
    let guardian = match store.lock().expect("poisoned").get_guardian(id) {
        Ok(g) => g,
        Err(_) => return,
    };
    if cancel.is_cancelled() {
        return;
    }
    let mut project_order: Vec<String> = Vec::new();
    let mut project_branches: std::collections::HashMap<String, Vec<crate::guardian::BranchView>> =
        std::collections::HashMap::new();
    for b in guardian.branches.iter().filter(|b| b.enabled) {
        let proj = b
            .project
            .clone()
            .unwrap_or_else(|| guardian.git_root.clone());
        if !project_branches.contains_key(&proj) {
            project_order.push(proj.clone());
        }
        project_branches.entry(proj).or_default().push(b.clone());
    }
    for branches in project_branches.values_mut() {
        branches.sort_by_key(|b| b.position);
    }
    if project_order.is_empty() {
        // All branches disabled — review is a no-op.
        set_status(
            GuardianStatus::InReview,
            Some("all branches disabled — review is a no-op"),
        );
        return;
    }

    let mut last_combined: Option<String> = None;
    let mut last_root: Option<Workspace> = None;
    for proj in &project_order {
        if cancel.is_cancelled() {
            log_merge_cancelled(store, id);
            return;
        }
        let root = Workspace::for_guardian(store, id, PathBuf::from(&proj));
        let wt_base = root.at(worktree_dir(proj, id));
        // The base commit this project's stack was actually rebased onto, as
        // recorded by [`staged_merge_pass`] before it built -- deliberately not
        // a fresh resolve. The base branch can advance while a pass is still
        // resolving conflicts and running proofs; overwriting the baseline with
        // that newer tip would leave `rebuild_on_base_shift` comparing equal, so
        // the review would sit in `in_review` permanently stale against a base
        // it was never built on. Keeping the built-against commit lets the
        // maintenance sweep see the shift and rebuild.
        let base_sha = match guardian.base_commits.get(proj) {
            Some(sha) => sha.clone(),
            None => {
                let sha = match resolve_base(&root, &guardian.base_branch) {
                    Ok(s) => s,
                    Err(e) => {
                        set_status(
                            GuardianStatus::MergeFailed,
                            Some(&format!(
                                "{proj}: base branch '{}': {e}",
                                guardian.base_branch
                            )),
                        );
                        return;
                    }
                };
                let _ = store
                    .lock()
                    .expect("poisoned")
                    .set_guardian_project_base_commit(id, proj, &sha);
                sha
            }
        };
        // Combined worktree points at the head of this project's last branch.
        let prev_ref = project_branches[proj]
            .last()
            .map(|b| review_ref_of(id, b))
            .unwrap_or_else(|| base_sha.clone());
        match rebuild_combined(store, &root, &wt_base, id, &prev_ref) {
            Ok(combined_str) => {
                last_combined = Some(combined_str);
                last_root = Some(root.clone());
                generate_manual_commands(
                    store,
                    runner,
                    id,
                    &root,
                    &base_sha,
                    &prev_ref,
                    Some(&wt_base.join("review")),
                    cancel,
                );
            }
            Err(e) => {
                set_status(GuardianStatus::MergeFailed, Some(&e));
                return;
            }
        }
    }
    if cancel.is_cancelled() {
        log_merge_cancelled(store, id);
        return;
    }
    let note = if let (Some(combined_str), Some(root)) = (&last_combined, &last_root) {
        match final_checks(store, runner, id, root, combined_str, cancel) {
            Ok(n) => n,
            Err(e) => {
                set_status(GuardianStatus::MergeFailed, Some(&e));
                return;
            }
        }
    } else {
        None
    };
    // RAL-92: baseline the freshly-built review-branch tips so this build is
    // never read as a reviewer's manual push on the next maintenance sweep.
    snapshot_review_heads(store, id);
    // RAL-208: debounced LLM change summary for the whole guardian, now that
    // every branch has finished rebuilding.
    queue_final_summary_regen(store, id);
    set_status(GuardianStatus::InReview, note.as_deref());
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
///
/// RAL-213: `cancel` lets a guardian-settings change (or an explicit
/// "cancel and restart") stop an in-flight merge before the next checkpoint,
/// so a stale build never fights a fresh one over the same worktrees/branches
/// (see [`restart_guardian_merge`]). On cancellation this returns immediately
/// at the next checkpoint without any further status write or destructive git
/// operation -- the next merge's own setup phase (the reset-to-pending +
/// `cleanup_review_worktrees` pass above) already handles cleaning up
/// whatever this pass left behind.
pub fn run_merge_cancellable(
    store: &Arc<Mutex<Store>>,
    runner: &dyn Runner,
    id: &str,
    cancel: &CancelToken,
) {
    let guardian = match store.lock().expect("poisoned").get_guardian(id) {
        Ok(g) => g,
        Err(_) => return,
    };
    // RAL-193: every call is its own merge/rebase attempt -- bump the
    // counter so cost line items recorded during it (conflict resolution,
    // proving) are attributed to this attempt, distinct from the
    // cumulative total across every attempt this review has gone through.
    let _ = store
        .lock()
        .expect("poisoned")
        .bump_guardian_merge_attempt(id);
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
            squad_id: None,
            guardian_id: Some(id),
            cell_id: None,
            task: None,
            log_path: None,
            payload: serde_json::json!({
                "base": base,
                "branches": guardian.branches.iter().filter(|b| b.enabled).count(),
            }),
            admin_only: false,
        });
    }

    let set_status = |s: GuardianStatus, detail: Option<&str>| {
        let _ = store
            .lock()
            .expect("poisoned")
            .set_guardian_status(id, s, detail);
    };
    let resolved = match resolve_resolver_agent(
        guardian.resolver_agent.as_deref(),
        guardian.resolver_model.as_deref(),
        Path::new(&guardian.git_root),
    ) {
        Ok(resolved) => resolved,
        Err(error) => {
            set_status(
                GuardianStatus::MergeFailed,
                Some(&format!("unresolvable resolver agent: {error}")),
            );
            return;
        }
    };
    if let Err(error) = preflight_resolver_agent(runner, &resolved, guardian.machine.as_deref()) {
        set_status(GuardianStatus::MergeFailed, Some(&error));
        return;
    }
    set_status(GuardianStatus::Merging, None);
    {
        let guard = store.lock().expect("poisoned");
        // RAL-103: the change summary is deliberately NOT cleared here -- the
        // last computed summary (preliminary or final) stays visible until
        // a debounced `generate_final_summary` (RAL-208) eventually overwrites
        // it, instead of showing a misleading empty/"generating" gap for the
        // whole rebuild.
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

    // RAL-168: unambiguous "last branch in the stack" for `ProofScope::FinalBranch`,
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
            let rev = review_ref_of(id, ob);
            if let Ok(sha) = proot.git(&["rev-parse", "--verify", &rev]) {
                let sha = sha.trim().to_string();
                carry.pin(proot.root(), id, &ob.position.to_string(), &sha);
                old_review.insert((proj.clone(), ob.branch.clone()), sha);
            }
        }
    }

    // Clean up prior worktrees for ALL projects before starting fresh. The
    // claimed readable names are deleted alongside the internal refs (RAL-378);
    // the carry-forward pins taken just above keep their commits reachable, the
    // same way they already did for the internal refs.
    for (proj, _) in &project_branches {
        let root = ws_root.at(PathBuf::from(proj));
        let wt_base = ws_root.at(worktree_dir(proj, id));
        let claimed = claimed_review_branches(store, id, Some(proj));
        cleanup_review_worktrees(&root, &wt_base, id, &num, &claimed);
    }

    // Track the last combined worktree (and its project root, for RAL-101
    // auto-build config resolution) across all projects, used for final checks.
    let mut last_combined: Option<String> = None;
    let mut last_root: Option<Workspace> = None;

    for (proj, proj_branches) in &project_branches {
        if cancel.is_cancelled() {
            log_merge_cancelled(store, id);
            return;
        }
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

        if let Err(e) = preflight_worktree_budget(&root, &wt_base, &base_sha) {
            set_status(GuardianStatus::MergeFailed, Some(&e));
            return;
        }

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
                    readable_review_branch: b.readable_review_branch,
                    review_branch_name: b.review_branch_name.clone(),
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
                cancel,
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
        // RAL-211: short worktree-directory names for this project's branches
        // -- see `branch_short_names`'s doc comment for the stability
        // requirement this depends on.
        let short_names = branch_short_names(store, id, Some(proj.as_str()));
        for ob in proj_branches {
            if cancel.is_cancelled() {
                log_merge_cancelled(store, id);
                return;
            }
            let _ = store.lock().expect("poisoned").set_branch_status(
                id,
                &ob.id,
                MergeStatus::InProgress,
                None,
            );
            // RAL-185 Phase 3b: a branch whose cell ran on another machine
            // has its commits over there, not here -- pull them in before the
            // stack tries to use them.
            if let Err(e) = fetch_branch_for_remote_cell(store, id, ob) {
                fail_branch(store, id, &ob.id, &ob.branch, &e, &set_status);
                return;
            }
            let rev = match claim_branch_review_ref(
                store,
                &root,
                id,
                proj,
                &ob.id,
                &ob.branch,
                ob.readable_review_branch,
                ob.review_branch_name.as_deref(),
            ) {
                Ok(rev) => rev,
                Err(e) => {
                    fail_branch(store, id, &ob.id, &ob.branch, &e, &set_status);
                    return;
                }
            };
            let wt = branch_wt_dir(&wt_base, &short_names, &ob.branch);
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
            let gate = ProofGate::resolve(
                store,
                id,
                Some(ob.id.as_str()) == final_branch_id.as_deref(),
            );
            if cancel.is_cancelled() {
                log_merge_cancelled(store, id);
                return;
            }
            if stack_pick(
                store, runner, id, &ob.id, &ob.branch, &upstream, &prev_ref, &rev, &wt, squash,
                &gate, cancel,
            )
            .is_err()
            {
                return;
            }
            if cancel.is_cancelled() {
                log_merge_cancelled(store, id);
                return;
            }
            if let Err(e) = run_commit_checks(store, id, &ob.id, &wt, &ob.branch, cancel) {
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
                    "branch is empty: it adds no changes over the branch beneath it in the stack.                      Its task most likely never committed its work -- check that cell, then re-run                      it. If this branch is meant to be empty, disable it to drop it from the stack.",
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
                // RAL-27: regenerate manual review commands once the stack is
                // ready for this project.
                generate_manual_commands(
                    store,
                    runner,
                    id,
                    &root,
                    &base_sha,
                    &prev_ref,
                    Some(&root.at(&combined_wt)),
                    cancel,
                );
            }
            Err(e) => {
                set_status(GuardianStatus::MergeFailed, Some(&e));
                return;
            }
        }
    }

    if cancel.is_cancelled() {
        log_merge_cancelled(store, id);
        return;
    }
    // Run final check gates against the last combined worktree (all-projects pass).
    let note = if let (Some(combined_str), Some(root)) = (&last_combined, &last_root) {
        match final_checks(store, runner, id, root, combined_str, cancel) {
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
    // RAL-208: every enabled branch across every project has now finished
    // rebuilding -- request a (debounced, dedup'd-by-signature) LLM change
    // summary covering the whole guardian, once, instead of the old
    // per-project unconditional call this replaced.
    queue_final_summary_regen(store, id);
    set_status(GuardianStatus::InReview, note.as_deref());
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
    cancel: &CancelToken,
) {
    let combined_branch = match claim_combined_review_ref_by_id(store, root, id) {
        Ok(name) => name,
        Err(e) => {
            set_status(GuardianStatus::MergeFailed, Some(&e));
            return;
        }
    };
    let wt = wt_base.join("review");
    let wt_str = wt.root().to_string_lossy().to_string();
    if let Err(e) = worktree_add_or_reset(root, &combined_branch, &wt, base_sha) {
        set_status(GuardianStatus::MergeFailed, Some(&e));
        return;
    }
    for ob in branches {
        if cancel.is_cancelled() {
            log_merge_cancelled(store, id);
            return;
        }
        let _ = store.lock().expect("poisoned").set_branch_status(
            id,
            &ob.id,
            MergeStatus::InProgress,
            None,
        );
        // Every branch shares the one combined worktree/branch; record it now so
        // the UI can show the expand row and feedback widget even if this branch fails.
        //
        // RAL-378: deliberately no per-branch `review_branch_name` claim here.
        // N branches sharing one ref cannot each own a distinct PR branch, so
        // this path keeps deriving each PR's alias from the convention exactly
        // as it did before -- `resolve_pr_alias` sees `None` for the readable
        // name and takes its separated path regardless of `separate_pr_branch`.
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
        let gate = ProofGate::resolve(store, id, Some(ob.id.as_str()) == final_branch_id);
        match drive_rebase(
            store,
            id,
            &ob.id,
            runner,
            &ob.branch,
            &wt,
            &combined_branch,
            base_sha,
            "HEAD",
            &gate,
            cancel,
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
                    // RAL-168: proofed but no conflict occurred -- still `Done`.
                    RebaseOutcome::CleanProofed(note) => (MergeStatus::Done, Some(note)),
                    RebaseOutcome::Clean => (
                        MergeStatus::Done,
                        nothing.then(|| "no new commits over base (already merged?)".to_string()),
                    ),
                };
                promote_branch_terminal(
                    store,
                    runner,
                    id,
                    &ob.id,
                    status,
                    detail.as_deref(),
                    session_id.as_deref(),
                );
            }
            Err(e) => {
                // RAL-213: a cancelled merge is already logged by `drive_rebase`'s
                // own checkpoint -- leave the worktree/branch state as-is for the
                // next merge's own setup phase to clean up.
                if cancel.is_cancelled() {
                    return;
                }
                // drive_rebase already aborted; restore the combined branch.
                let _ = wt.git(&["checkout", "--force", &combined_branch]);
                fail_branch(store, id, &ob.id, &ob.branch, &e, set_status);
                return;
            }
        }
        if cancel.is_cancelled() {
            log_merge_cancelled(store, id);
            return;
        }
        if let Err(e) = run_commit_checks(store, id, &ob.id, &wt, &ob.branch, cancel) {
            fail_branch(store, id, &ob.id, &ob.branch, &e, set_status);
            return;
        }
    }
    if cancel.is_cancelled() {
        log_merge_cancelled(store, id);
        return;
    }
    {
        let guard = store.lock().expect("poisoned");
        let _ = guard.set_guardian_review_branch(id, &combined_branch);
        let _ = guard.set_guardian_combined_worktree(id, &wt_str);
    }
    regenerate_readme(store, root);
    match final_checks(store, runner, id, root, &wt_str, cancel) {
        Ok(note) => {
            // RAL-208: the LLM change summary is no longer regenerated here on
            // every stack rebuild -- `run_merge` requests a (debounced) regen
            // once, after every project in this merge has finished, so it is
            // never re-triggered by a rebuild that didn't add/remove/enable/
            // disable a branch (a feedback restack, a manual-push rebase, a
            // base-shift rebuild).
            // RAL-27: generate manual review commands once the stack is ready.
            generate_manual_commands(
                store,
                runner,
                id,
                root,
                base_sha,
                &combined_branch,
                Some(&wt),
                cancel,
            );
            // RAL-92: baseline the shared review branch's tip (all branches share
            // it here) so the daemon's own build is not read as a manual push.
            snapshot_review_heads(store, id);
            set_status(GuardianStatus::InReview, note.as_deref());
        }
        Err(e) => set_status(GuardianStatus::MergeFailed, Some(&e)),
    }
}

/// What a [`run_feedback`] pass actually did to the target branch's own
/// commit/push -- used by [`crate::pr::action_pr_feedback_inner`] to record
/// the pushed sha against a PR row without re-implementing the push itself
/// (RAL-<new>). Every early-exit path before a commit is attempted returns
/// [`Self::default`] (all `false`/`None`).
#[derive(Debug, Clone, Default)]
pub struct FeedbackOutcome {
    /// Whether the resolver agent's edits were committed onto the branch.
    pub committed: bool,
    /// The resulting commit sha, when `committed`.
    pub sha: Option<String>,
    /// Whether that commit was pushed to a remote.
    pub pushed: bool,
    /// The pushed sha, when `pushed` (equal to `sha`).
    pub pushed_sha: Option<String>,
}

/// Apply reviewer `feedback` to one branch's review worktree (via the agent);
/// if it made real edits, run the same dedicated final-proof pass a clean
/// rebase gets (unless skipped), commit it onto that branch's review branch
/// (amending in place when the branch's project has squash-to-one-commit
/// enabled, else a new commit), push the branch (force only when the commit
/// was NOT amended), then re-stack the downstream branches on top and
/// rebuild the combined worktree. The task worktrees are never touched.
/// Deliberately not PR-system-aware -- see [`push_feedback_branch`]'s doc
/// comment. Runs synchronously (spawned by [`start_feedback`]).
pub fn run_feedback(
    store: &Arc<Mutex<Store>>,
    runner: &dyn Runner,
    id: &str,
    branch_id: &str,
    feedback: &str,
    cancel: &CancelToken,
) -> FeedbackOutcome {
    let guardian = match store.lock().expect("poisoned").get_guardian(id) {
        Ok(g) => g,
        Err(_) => return FeedbackOutcome::default(),
    };
    let base = guardian.base_branch.clone();
    let Some(branch) = guardian.branches.iter().find(|b| b.id == branch_id) else {
        return FeedbackOutcome::default();
    };
    // RAL-375: persist the feedback text durably before any work begins, so
    // an unclean shutdown while this function is still running (the resolver
    // agent, the push, or the downstream restack below) leaves a record
    // startup recovery can find and reapply -- previously this text existed
    // only as this function's own argument, so an interrupted feedback round
    // was silently lost even after the guardian's merge later resumed
    // normally. Cleared at every real exit point below.
    let _ = store
        .lock()
        .expect("poisoned")
        .set_branch_pending_feedback(id, branch_id, feedback);
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
            squad_id: None,
            guardian_id: Some(id),
            cell_id: None,
            task: None,
            log_path: None,
            payload: serde_json::json!({"position": position}),
            admin_only: false,
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
        let _ = store
            .lock()
            .expect("poisoned")
            .clear_branch_pending_feedback(id, branch_id);
        set_status(
            GuardianStatus::MergeFailed,
            Some("no review worktree yet; run the merge first"),
        );
        return FeedbackOutcome::default();
    };
    let feature = branch.branch.clone();
    let review_branch = branch
        .review_branch
        .clone()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| review_ref_of(id, branch));
    // RAL-91: same squash-membership check the downstream restack loop uses
    // below, computed once here so the target branch's own commit can also
    // respect it.
    let squash = guardian
        .squash_projects
        .iter()
        .any(|p| p == &branch_project);
    let is_final_branch = guardian
        .branches
        .iter()
        .filter(|b| b.enabled)
        .max_by_key(|b| b.position)
        .is_some_and(|b| b.id == branch_id);
    let wt = root.at(PathBuf::from(&wt_str));
    set_status(GuardianStatus::Merging, None);

    // The agent edits the review worktree; we commit onto its review branch.
    let prompt = format!(
        "You are revising branch '{feature}' in response to reviewer feedback. \
         Edit the files in this worktree to satisfy the feedback, then stop. \
         Feedback: {feedback}. Do not run any git commands."
    );
    let resolved = match resolve_resolver_agent(
        guardian.resolver_agent.as_deref(),
        guardian.resolver_model.as_deref(),
        Path::new(&branch_project),
    ) {
        Ok(r) => r,
        Err(message) => {
            let _ = store
                .lock()
                .expect("poisoned")
                .clear_branch_pending_feedback(id, branch_id);
            set_status(
                GuardianStatus::MergeFailed,
                Some(&format!("unresolvable resolver agent: {message}")),
            );
            return FeedbackOutcome::default();
        }
    };
    // RAL-<new>: flip the TARGET branch's own status to Actioning for the
    // resolver's duration -- until now `run_feedback` never touched this
    // branch's own `merge_status` at all, so the board gave no visible sign
    // feedback was received/being worked. Must be set here, not earlier --
    // the two failure exits above never touched the branch's status, so
    // there's nothing to unwind if they fire.
    let _ = store.lock().expect("poisoned").set_branch_status(
        id,
        branch_id,
        MergeStatus::Actioning,
        Some("applying reviewer feedback"),
    );
    let spec = RunnerSpec {
        // RAL-102: unique per guardian — a bare "guardian" squad_id collides
        // with every other guardian's tmux session name (see the identical
        // fix on `generate_final_summary`'s spec).
        squad_id: format!("guardian-{id}"),
        task: FEEDBACK_TASK.to_string(),
        cell_id: feedback_cell_id(branch_id),
        cwd: wt_str,
        prompt: Some(prompt),
        command: None,
        agent: resolved.backend.clone(),
        executable: resolved.executable.clone(),
        model: resolved.model.clone(),
        system_prompt: None,
        system_prompt_position: None,
        timeout_sec: None,
        budget_tokens: None,
        maximum_budget_usd: None,
        maximum_context: None,
        auto_compact_threshold: None,
        maximum_tool_output_tokens: None,
        proof: false,
        trace_context: None,
        resume_agent_session_id: None,
        assigned_agent_session_id: None,
        env_overrides: resolved.env.clone(),
        // RAL-201: route to the same machine `wt` (and thus `cwd` above) is
        // actually on -- see the identical fix in
        // `resolve_conflicts_with_agent`.
        machine: wt.machine().map(str::to_string),
        tool_arg_truncate_chars: None,
        thrash_max_compactions: None,
        thrash_min_turn_gap: None,
        allow_personal_settings: false,
        allow_personal_memory: false,
    };
    let no_commit = is_no_commit_intent(feedback);
    // Stash any pre-existing dirty state so we only include the agent's own
    // changes in the new commit (not leftovers from a prior no-commit turn).
    // RAL-283: named + uniquified, not a bare `git stash` — this worktree's
    // stash lives on the shared `refs/stash` stack of the whole repo (git has
    // no per-worktree stash), so a bare push/pop here could collide with
    // another branch's feedback/rebase window on the same repo.
    let stash_name = if !no_commit {
        let pre = wt.git(&["status", "--porcelain"]).unwrap_or_default();
        if pre.trim().is_empty() {
            None
        } else {
            let name = crate::stash::unique_stash_name(
                &format!("guardian/{id}/{review_branch}"),
                "feedback",
            );
            wt.git(&["stash", "push", "--include-untracked", "--message", &name])
                .ok()
                .map(|_| name)
        }
    } else {
        None
    };
    let result = runner.run_cancellable(&spec, cancel);
    let _ = record_guardian_call_cost(store, id, Some(branch_id), "feedback", &result);
    let dirty = wt.git(&["status", "--porcelain"]).unwrap_or_default();
    let committed = !dirty.trim().is_empty() && !no_commit;
    let mut proof_note: Option<String> = None;
    let mut pushed = false;
    let mut pushed_sha: Option<String> = None;
    let mut push_error: Option<String> = None;
    if committed {
        let _ = wt.git(&["add", "--all"]);

        // RAL-<new>: give the target branch itself the same dedicated
        // final-proof pass a cleanly-rebased branch already gets during a
        // restack (see `drive_rebase`'s identical `allows_for_clean_branch`
        // call) -- a feedback revision is a routine edit, not a conflict
        // resolution, so `skip_auto_clean` applies here too.
        let gate = ProofGate::resolve(store, id, is_final_branch);
        if gate.allows_for_clean_branch() {
            let (quality_note, ghost_prefix) =
                proof_extras(store, id, &feature, branch_id, runner, &resolved, cancel);
            let (_, note) = run_final_proof(
                store,
                id,
                branch_id,
                runner,
                &wt,
                &feature,
                &resolved,
                &quality_note,
                &ghost_prefix,
                cancel,
            );
            proof_note = Some(note);
            // The proof pass may itself have edited files.
            let _ = wt.git(&["add", "--all"]);
        }

        // RAL-<new>: extend the branch's single squashed commit in place
        // rather than adding a new one when the project has squash-to-one-
        // commit enabled.
        if squash {
            let _ = wt.git(&["commit", "--amend", "--no-edit"]);
        } else {
            // RAL-201: was `git(wt.root(), ...)`, a direct bypass of `wt`'s
            // machine sitting right next to the correctly-routed calls above.
            // RAL-<new>: subject line stays short (this repo's conventional
            // `type: summary` commit style) with the raw reviewer feedback --
            // which routinely runs to several sentences -- relegated to the
            // commit body via a second `-m`, instead of dumping the whole
            // feedback text into the subject line where it makes
            // `git log --oneline` and rebase-todo listings unreadable.
            let subject = format!("fix: apply review feedback ({feature})");
            let _ = wt.git(&["commit", "--message", &subject, "--message", feedback]);
        }

        // RAL-<new>: push the review branch itself -- force only when we did
        // NOT amend (a plain new commit may not fast-forward the remote's
        // previous review push; an amend is a routine extension of history
        // the remote already expects to be rewritten).
        // RAL-338: resolve the fork remote explicitly, if this branch's
        // project has one registered, rather than letting
        // `push_feedback_branch` infer it through `@{upstream}`.
        let fork_remote =
            crate::pr::resolve_feedback_fork_remote(store, Path::new(&branch_project));
        match push_feedback_branch(&wt, &review_branch, !squash, fork_remote.as_deref()) {
            Ok(sha) => {
                pushed = true;
                pushed_sha = Some(sha);
            }
            Err(e) => push_error = Some(e),
        }
    }
    let sha = if committed {
        wt.git(&["rev-parse", "HEAD"])
            .ok()
            .map(|s| s.trim().to_string())
    } else {
        None
    };
    // Restore any pre-existing (no-commit) changes to the working tree.
    if let Some(name) = &stash_name {
        if let Err(e) = crate::stash::pop_named(|args| wt.git(args), name) {
            crate::rlog!(
                WARNING,
                "ralphus [guardian] review {id} feedback: stash restore failed: {e}"
            );
        }
    }
    // RAL-241 follow-up: every path here previously reported the same
    // "feedback applied" detail regardless of what actually happened --
    // an agent run that errored out, or one that simply left the worktree
    // untouched (with `no_commit` not requested), was indistinguishable
    // from a real fix, so a reviewer polling `review status` had no way to
    // tell a silent no-op from success without manually inspecting the
    // worktree's git history. `no_commit` reflects the feedback text's own
    // request to skip committing and is not a failure, so it keeps the
    // original wording.
    let (branch_status, detail) = if !result.is_done() {
        (
            MergeStatus::Failed,
            format!(
                "feedback failed: agent error: {}",
                result.error.as_deref().unwrap_or("unknown error")
            ),
        )
    } else if !committed && !no_commit {
        (
            MergeStatus::Done,
            "feedback: agent made no changes".to_string(),
        )
    } else if let Some(e) = &push_error {
        (
            MergeStatus::Failed,
            format!("feedback committed but push failed: {e}"),
        )
    } else {
        let mut msg = "feedback applied".to_string();
        if let Some(note) = &proof_note {
            msg.push_str(&format!("; {note}"));
        }
        if pushed {
            msg.push_str("; pushed");
        }
        (MergeStatus::Done, msg)
    };
    let _ = store.lock().expect("poisoned").set_branch_status(
        id,
        branch_id,
        branch_status,
        Some(&detail),
    );
    // RAL-375: this is a real completion (success or a legitimate failure),
    // not a crash -- clear the durable pending-feedback record set at the
    // top of this function so startup recovery doesn't try to reapply
    // feedback that already ran to completion.
    let _ = store
        .lock()
        .expect("poisoned")
        .clear_branch_pending_feedback(id, branch_id);
    // RAL-<new>: a feedback revision can push a real new commit onto the
    // branch's review ref, so it needs the same auto-submit hook every other
    // route to a terminal status fires via `promote_branch_terminal` --
    // otherwise an already-open PR with `auto_submit_pr_stack` on is left
    // pointed at the pre-feedback sha. Gated on `Done` only, matching
    // `promote_branch_terminal`/`fail_branch`'s existing split: auto-submit
    // follows a real terminal success, never a failure.
    if branch_status == MergeStatus::Done {
        crate::pr::maybe_auto_submit_branch(store, runner, id, branch_id);
        // RAL-375: a feedback push onto a branch that already has (or just
        // gained, via the auto-submit call just above) an open PR should
        // start watching that PR's CI/mergeability -- gated on `pushed`
        // since a feedback pass that only reports (no worktree change, or
        // `no_commit` requested) has nothing new on the forge to watch.
        if pushed {
            crate::ci_watch::watch_after_feedback_push(store, id, branch_id);
        }
    }
    let outcome = FeedbackOutcome {
        committed,
        sha,
        pushed,
        pushed_sha,
    };
    crate::rlog!(
        INFO,
        "ralphus [guardian] review {id} feedback done position={position} no_commit={no_commit} committed={committed}"
    );
    {
        let guard = store.lock().expect("poisoned");
        let _ = guard.cartographer_log(crate::cartographer::CartographerEntry {
            level: crate::logging::LogLevel::INFO,
            source: "guardian",
            message: "feedback done",
            scope: Some("branch"),
            squad_id: None,
            guardian_id: Some(id),
            cell_id: None,
            task: None,
            log_path: None,
            payload: serde_json::json!({
                "position": position,
                "no_commit": no_commit,
                "committed": committed,
            }),
            admin_only: false,
        });
    }
    if no_commit {
        // RAL-92: no commit was created so the review-branch tips are unchanged;
        // re-baseline anyway to keep manual-push detection consistent.
        snapshot_review_heads(store, id);
        set_status(GuardianStatus::InReview, None);
        return outcome;
    }

    // Re-affirm `Merging` before restacking downstream: the target branch's
    // resolver call above can run long enough for a concurrent failure
    // elsewhere (e.g. a racing "Merge / rebase" click hitting a transient
    // worktree error) to overwrite the guardian to `MergeFailed` in the
    // meantime. Without this, nothing corrects it back for the rest of this
    // restack -- every downstream branch below keeps advancing to
    // `done`/`conflict_resolved` while the top-level status stays stuck on
    // the stale failure until `finalize_review` finally overwrites it below.
    set_status(GuardianStatus::Merging, None);

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
            return outcome;
        }
    };
    let all_branches = store
        .lock()
        .expect("poisoned")
        .get_guardian(id)
        .map(|g| g.branches)
        .unwrap_or_default();
    // RAL-168: unambiguous "last branch in the stack" for `ProofScope::FinalBranch`
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
    // RAL-91: downstream branches share this branch's project, so the
    // `squash` computed above (for the target branch's own commit) is
    // constant across the re-stack -- reused here rather than recomputed.
    // RAL-211: short worktree-directory names for this project's branches --
    // see `branch_short_names`'s doc comment for the stability requirement
    // this depends on.
    let short_names = branch_short_names(store, id, Some(branch_project.as_str()));
    let mut prev_ref = branch
        .review_branch
        .clone()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| review_ref_of(id, branch));
    for ob in &downstream {
        let _ = store.lock().expect("poisoned").set_branch_status(
            id,
            &ob.id,
            MergeStatus::InProgress,
            None,
        );
        let rev = match claim_branch_review_ref(
            store,
            &root,
            id,
            &branch_project,
            &ob.id,
            &ob.branch,
            ob.readable_review_branch,
            ob.review_branch_name.as_deref(),
        ) {
            Ok(rev) => rev,
            Err(e) => {
                fail_branch(store, id, &ob.id, &ob.branch, &e, &set_status);
                return outcome;
            }
        };
        let wt_j = branch_wt_dir(&wt_base, &short_names, &ob.branch);
        let wt_j_str = wt_j.root().to_string_lossy().to_string();
        // Reset the review branch to the feature tip; drive_rebase replays its
        // own commits onto the revised upstream (`prev_ref`).
        if let Err(e) = worktree_add_or_reset(&root, &rev, &wt_j, &ob.branch) {
            fail_branch(store, id, &ob.id, &ob.branch, &e, &set_status);
            return outcome;
        }
        let _ = store
            .lock()
            .expect("poisoned")
            .set_branch_review(id, &ob.id, &rev, &wt_j_str);
        let gate = ProofGate::resolve(
            store,
            id,
            Some(ob.id.as_str()) == final_branch_id.as_deref(),
        );
        // RAL-213/RAL-239: this downstream re-stack is its own invocation, not
        // the background merge worker `run_merge_cancellable` guards against
        // racing -- but it still must honor an explicit review cancel, so it
        // shares this feedback application's own live `cancel` token rather
        // than a token that never trips.
        if stack_pick(
            store, runner, id, &ob.id, &ob.branch, &base_sha, &prev_ref, &rev, &wt_j, squash,
            &gate, cancel,
        )
        .is_err()
        {
            return outcome;
        }
        if let Err(e) = run_commit_checks(store, id, &ob.id, &wt_j, &ob.branch, cancel) {
            fail_branch(store, id, &ob.id, &ob.branch, &e, &set_status);
            return outcome;
        }
        prev_ref = rev;
    }

    match finalize_review(store, runner, &root, &wt_base, id, &prev_ref, cancel) {
        Ok(note) => {
            // RAL-208: request a change-summary regen -- a no-op unless the
            // enabled-branch set actually changed since the last one (a
            // feedback restack never does), and debounced when it did.
            queue_final_summary_regen(store, id);
            // RAL-27: regenerate manual review commands after the re-stack.
            generate_manual_commands(
                store,
                runner,
                id,
                &root,
                &base_sha,
                &prev_ref,
                Some(&wt_base.join("review")),
                cancel,
            );
            // RAL-92: re-baseline after applying feedback so the new tips (the
            // edited branch and its restacked downstream) are the reference for
            // future manual-push detection.
            snapshot_review_heads(store, id);
            set_status(GuardianStatus::InReview, note.as_deref());
        }
        Err(e) => set_status(GuardianStatus::MergeFailed, Some(&e)),
    }
    outcome
}

/// Fetch `alias` from `remote` and rebase branch `branch_id`'s own unique
/// commits (since `last_synced_sha`, or their merge-base with the fetched tip
/// when `last_synced_sha` is unknown or stale) onto the fetched PR-branch tip
/// (RAL-190) — driving the agent through any conflicts exactly like a normal
/// stack rebase, so a reviewer's direct push to the open PR branch flows back
/// into the review worktree instead of being silently discarded on the next
/// force-push. On success, restacks everything downstream of this branch
/// (same restack [`rebase_on_manual_push`] performs after a detected manual
/// push) and re-baselines. Returns `Ok(false)` when the fetched tip was
/// already contained in the branch's history — nothing to pull.
///
/// Delegated to from `crate::pr::pull_pr_commits`, which owns fetching the
/// PR row, calling this, and pushing the merged result back to the remote
/// afterward (only this crate module has the rebase/conflict-resolution/
/// restack machinery `pr.rs` needs to reuse — see that module's doc comment
/// on why it delegates to `run_feedback` for the analogous text-feedback
/// case).
///
/// RAL-307 (`resolve_pr_alias`'s `use_worktree_branch_name`) needs no special
/// case here even though `alias` can now equal the feature branch's own
/// name: `remote`/`alias` is a REMOTE ref, `review_ref` (this worktree's
/// checked-out branch) is always the distinct, guardian-id-prefixed local
/// ref `submit_stacked_branch_pr` pushed to it -- they can never collide by
/// name. Reconciliation below is already purely SHA-based (fetch into the
/// anonymous `FETCH_HEAD`, `merge-base --is-ancestor` for the no-op check,
/// then a normal rebase), never a same-named self-rebase.
pub fn pull_pr_commits(
    store: &Arc<Mutex<Store>>,
    runner: &dyn Runner,
    id: &str,
    branch_id: &str,
    remote: &str,
    alias: &str,
    last_synced_sha: Option<&str>,
) -> std::result::Result<bool, String> {
    let guardian = store
        .lock()
        .expect("poisoned")
        .get_guardian(id)
        .map_err(|e| e.to_string())?;
    let branch = guardian
        .branches
        .iter()
        .find(|b| b.id == branch_id)
        .cloned()
        .ok_or_else(|| format!("no branch with id {branch_id}"))?;
    let position = branch.position;
    let branch_project = branch
        .project
        .clone()
        .unwrap_or_else(|| guardian.git_root.clone());
    let root = Workspace::for_guardian(store, id, PathBuf::from(&branch_project));
    let Some(wt_str) = branch.worktree.clone() else {
        return Err("no review worktree yet; run the merge first".to_string());
    };
    let Some(review_ref) = branch.review_branch.clone() else {
        return Err("no review worktree yet; run the merge first".to_string());
    };
    let wt = root.at(PathBuf::from(&wt_str));

    root.git(&["fetch", remote, alias])
        .map_err(|e| format!("fetch {remote}/{alias} failed: {e}"))?;
    let fetched = root
        .git(&["rev-parse", "FETCH_HEAD"])
        .map_err(|e| e.to_string())?
        .trim()
        .to_string();
    let current_tip = wt
        .git(&["rev-parse", "HEAD"])
        .map_err(|e| e.to_string())?
        .trim()
        .to_string();
    if root
        .git(&["merge-base", "--is-ancestor", &fetched, &current_tip])
        .is_ok()
    {
        return Ok(false);
    }

    let base_for_rebase = match last_synced_sha {
        Some(sha) if root.git(&["cat-file", "--exists", sha]).is_ok() => sha.to_string(),
        _ => root
            .git(&["merge-base", &current_tip, &fetched])
            .map_err(|e| format!("no common history with fetched PR branch: {e}"))?
            .trim()
            .to_string(),
    };

    let claimed = {
        let g = store.lock().expect("poisoned");
        matches!(g.get_guardian(id), Ok(gv) if gv.status.as_str() == "in_review")
            && g.set_guardian_status(
                id,
                GuardianStatus::Merging,
                Some("pulling PR branch commits"),
            )
            .is_ok()
    };
    if !claimed {
        return Err(
            "review is not idle (in_review) -- cannot pull PR commits right now".to_string(),
        );
    }

    crate::rlog!(
        INFO,
        "ralphus [guardian] review {id} pulling pr commits branch={branch_id} \
         from={remote}/{alias} fetched={fetched}"
    );
    {
        let guard = store.lock().expect("poisoned");
        let _ = guard.cartographer_log(crate::cartographer::CartographerEntry {
            level: crate::logging::LogLevel::INFO,
            source: "pr",
            message: "pulling pr commits into worktree",
            scope: Some("branch"),
            squad_id: None,
            guardian_id: Some(id),
            cell_id: None,
            task: None,
            log_path: None,
            payload: serde_json::json!({
                "branch_id": branch_id,
                "remote": remote,
                "alias": alias,
                "fetched": fetched,
            }),
            admin_only: false,
        });
    }

    let set_status = |s: GuardianStatus, detail: Option<&str>| {
        let _ = store
            .lock()
            .expect("poisoned")
            .set_guardian_status(id, s, detail);
    };
    let final_branch_id: Option<String> = guardian
        .branches
        .iter()
        .filter(|b| b.enabled)
        .max_by_key(|b| b.position)
        .map(|b| b.id.clone());
    let gate = ProofGate::resolve(store, id, Some(branch_id) == final_branch_id.as_deref());
    // RAL-213: pulling reviewer-pushed PR commits is a separate flow from a
    // guardian-settings-triggered merge restart -- see
    // `run_merge_cancellable`'s doc comment.
    match drive_rebase(
        store,
        id,
        branch_id,
        runner,
        &branch.branch,
        &wt,
        &fetched,
        &base_for_rebase,
        &review_ref,
        &gate,
        &CancelToken::never(),
    ) {
        Ok(_) => {
            let wt_base = root.at(worktree_dir(&branch_project, id));
            restack_from_position(
                store,
                runner,
                id,
                &root,
                &wt_base,
                &guardian.base_branch,
                position,
                &set_status,
                &CancelToken::never(),
            );
            Ok(true)
        }
        Err(e) => {
            set_status(
                GuardianStatus::MergeFailed,
                Some(&format!("pulling PR commits: {e}")),
            );
            Err(e)
        }
    }
}

/// Poll every review for a base-branch shift and rebuild any that drifted, each
/// on its own thread. Called periodically by the scheduler loop so that new
/// commits landing on a review's base branch are picked up automatically.
/// Sweep all `in_review`/`merge_failed` guardians and rebuild any whose base
/// branch has shifted. Each spawned worker acquires a slot from `sem` only if
/// it actually decides to rebuild, so this never blocks unnecessarily.
pub fn review_maintenance(
    store: &Arc<Mutex<Store>>,
    sem: &Arc<Semaphore>,
    cancellations: &Cancellations,
) {
    let straggler_ids: Vec<String> = {
        let guard = store.lock().expect("poisoned");
        guard.guardians_with_ready_stragglers().unwrap_or_default()
    };
    for id in straggler_ids {
        let store = Arc::clone(store);
        let sem = Arc::clone(sem);
        let cancellations = cancellations.clone();
        std::thread::spawn(move || {
            let runner = crate::runner::SubprocessRunner::from_env();
            // RAL-213: register/remove around the merge this may trigger, same
            // shape as `scheduler::tick`'s squad-level wrapping, so a guardian
            // -settings change made while this reopen is rebuilding can stop it.
            let token = cancellations.register(&format!("guardian:{id}"));
            reopen_straggler(&store, &runner, &id, &sem, &token);
            cancellations.remove(&format!("guardian:{id}"));
        });
    }

    let ids: Vec<String> = {
        let guard = store.lock().expect("poisoned");
        guard
            .list_guardians()
            .unwrap_or_default()
            .into_iter()
            // RAL-300: `merging`/`merge_stopped` are included too (beyond the
            // base-shift/manual-push targets below) purely so the PR-merge
            // check just below can catch a PR that merged out-of-band while
            // this review's own rebase/feedback pass is what's using its
            // worktrees right now -- `rebuild_on_base_shift`/
            // `rebase_on_manual_push` already self-gate on `in_review`/
            // `merge_failed` and simply no-op for the other two.
            .filter(|g| {
                matches!(
                    g.status.as_str(),
                    "in_review" | "merge_failed" | "merging" | "merge_stopped"
                )
            })
            .map(|g| g.id)
            .collect()
    };
    for id in ids {
        let store = Arc::clone(store);
        let sem = Arc::clone(sem);
        let cancellations = cancellations.clone();
        std::thread::spawn(move || {
            let runner: Arc<dyn Runner> = Arc::new(
                crate::runner::SubprocessRunner::from_env().with_cartographer(Arc::clone(&store)),
            );
            // RAL-213: one token covers both the base-shift rebuild and (if
            // that didn't run) the manual-push restack below -- either may
            // trigger a merge for this guardian id.
            let token = cancellations.register(&format!("guardian:{id}"));
            // RAL-300: ask "have the linked PRs merged?" before deciding to
            // rebase at all. When this approves the review outright (every
            // linked PR merged, review was idle in `in_review`) or drops a
            // stale mid-flight PR, it already updated guardian/PR state --
            // the base-shift/manual-push calls below re-read guardian.status
            // themselves and naturally no-op once it's no longer
            // `in_review`/`merge_failed`, so no extra branching is needed here.
            crate::pr::check_pr_merges(&store, &id);
            // A base-shift rebuild (full re-derive) subsumes any manual push via
            // carry-forward, so only look for a manual push when no rebuild ran.
            if !rebuild_on_base_shift(&store, runner.as_ref(), &id, &sem, &token) {
                rebase_on_manual_push(&store, runner.as_ref(), &id, &sem);
            }
            // RAL-285: a restack rebuilds branch worktree tips in place but
            // never touches the remote branches already-open PRs track, so an
            // open PR goes stale the moment its review is rebuilt. The restacks
            // above are only two of the paths that do this -- an explicit
            // merge, a restart-merge and a feedback run all settle a review the
            // same way -- so rather than notify from each, reconcile every
            // settled review here on the sweep that already visits it. Skips
            // out before any network call when nothing drifted.
            crate::pr::sync_open_pr_branches(&store, &id);
            repair_missing_final_summary(&store, &id);
            cancellations.remove(&format!("guardian:{id}"));
        });
    }
}

/// Reopen a single guardian stuck out of `collecting` (`in_review` or
/// `merge_failed`) with a straggler branch: a linked review (RAL-97/98) whose
/// branches arrive from separate squads can leave the guardian's later branch at
/// `merge_status = 'pending'` forever, because `try_start_ready_reviews_for_task`
/// only re-examines guardians still in `collecting` when a task completes
/// (`guardian.rs::collecting_guardians_for_cells`). If the guardian already
/// moved on (e.g. to `in_review`) before the straggler's squad finished, nothing
/// else ever revisits it. This periodic self-heal (called from
/// [`review_maintenance`]) promotes the now-done branch to `ready` and, if the
/// reopen claim wins, runs the staged merge so the completed prefix is reused
/// and the newly-ready straggler is appended. Returns whether a reopen actually
/// happened.
pub fn reopen_straggler(
    store: &Arc<Mutex<Store>>,
    runner: &dyn Runner,
    id: &str,
    sem: &Semaphore,
    cancel: &CancelToken,
) -> bool {
    let claimed = {
        let guard = store.lock().expect("poisoned");
        // Only reopen if there was actually a straggler to promote — otherwise
        // this would reopen (and rebuild) every in_review/merge_failed guardian
        // on every maintenance sweep for no reason.
        let promoted = guard.mark_ready_branches_with_done_cells(id).unwrap_or(0);
        promoted > 0 && guard.reopen_guardian_merge(id).unwrap_or(false)
    };
    if claimed {
        crate::rlog!(
            INFO,
            "ralphus [guardian] review {id} reopened: straggler branch ready"
        );
        let _permit = sem.acquire();
        run_merge_staged(store, runner, id, cancel);
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
                        squad_id: None,
                        guardian_id: Some(id),
                        cell_id: None,
                        task: None,
                        log_path: None,
                        payload: serde_json::json!({"position": position, "old": prev, "new": current}),
                        admin_only: false,
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
            squad_id: None,
            guardian_id: Some(id),
            cell_id: None,
            task: None,
            log_path: None,
            payload: serde_json::json!({"from_position": from_position}),
            admin_only: false,
        });
    }
    // RAL-213: a manual-push restack is a separate reviewer-driven flow, not a
    // cancellable merge -- see `run_merge_cancellable`'s doc comment for the
    // feature this token type serves.
    restack_from_position(
        store,
        runner,
        id,
        &git_root,
        &wt_base,
        &guardian.base_branch,
        from_position,
        &set_status,
        &CancelToken::never(),
    );
    true
}

/// Whether every enabled branch under `project` already has its review-branch
/// tip as an ancestor of `base_sha` (RAL-300) -- i.e. the base already
/// contains that branch's work, so rebasing onto it would have nothing left
/// to do. `false` (never "already merged") when there is nothing to check
/// (no enabled branches for this project) or any branch has no review-branch
/// tip yet -- missing information must never read as "safe to skip".
fn project_already_in_base(
    root: &Workspace,
    guardian: &crate::guardian::GuardianView,
    project: &str,
    base_sha: &str,
) -> bool {
    let branches: Vec<&crate::guardian::BranchView> = guardian
        .branches
        .iter()
        .filter(|b| b.enabled)
        .filter(|b| b.project.as_deref().unwrap_or(guardian.git_root.as_str()) == project)
        .collect();
    if branches.is_empty() {
        return false;
    }
    branches.iter().all(|b| {
        let review_ref_is_in_base = b
            .review_branch
            .as_deref()
            .filter(|s| !s.is_empty())
            .is_some_and(|rev| {
                root.git(&["merge-base", "--is-ancestor", rev, base_sha])
                    .is_ok()
            });
        let worktree_is_in_upstream = b.worktree.as_deref().is_some_and(|worktree| {
            crate::reviews::workspace_head_is_ancestor_of_upstream(
                &root.at(PathBuf::from(worktree)),
            )
        });
        review_ref_is_in_base || worktree_is_in_upstream
    })
}

/// Whether every project of `guardian` already has [`project_already_in_base`]
/// true against that project's *current* base (RAL-300) -- i.e. the base has
/// already absorbed every enabled branch's commits, whether or not a shift
/// was otherwise detected. `false` when there are no projects, or any
/// project's current base can't even be resolved.
fn guardian_base_already_has_every_branch(
    store: &Arc<Mutex<Store>>,
    id: &str,
    guardian: &crate::guardian::GuardianView,
) -> bool {
    if guardian.projects.is_empty() {
        return false;
    }
    guardian.projects.iter().all(|proj| {
        let root = Workspace::for_guardian(store, id, Path::new(proj));
        match resolve_base(&root, &guardian.base_branch) {
            Ok(sha) => project_already_in_base(&root, guardian, proj, &sha),
            Err(_) => false,
        }
    })
}

/// Approve `id` because [`guardian_base_already_has_every_branch`] (or the
/// equivalent per-project check inline in [`rebuild_on_base_shift`]) found
/// every enabled branch already landed on its base (RAL-300) -- shared so the
/// periodic sweep and a manual "Merge / rebase" trigger log/approve
/// identically. Only valid from `in_review` (mirrors `approve_guardian`'s one
/// legal transition); returns whether it approved.
fn approve_base_already_landed(store: &Arc<Mutex<Store>>, id: &str) -> bool {
    let approved = store.lock().expect("poisoned").approve_guardian(id).is_ok();
    if approved {
        crate::rlog!(
            INFO,
            "ralphus [guardian] review {id} approved: base branch already contains every branch's commits"
        );
        let guard = store.lock().expect("poisoned");
        let _ = guard.cartographer_log(crate::cartographer::CartographerEntry {
            level: crate::logging::LogLevel::INFO,
            source: "guardian",
            message: "approved: base branch already contains every branch's commits",
            scope: Some("guardian"),
            squad_id: None,
            guardian_id: Some(id),
            cell_id: None,
            task: None,
            log_path: None,
            payload: serde_json::json!({}),
            admin_only: false,
        });
    }
    approved
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
    cancel: &CancelToken,
) -> bool {
    let guardian = match store.lock().expect("poisoned").get_guardian(id) {
        Ok(g) => g,
        Err(_) => return false,
    };
    if !matches!(guardian.status.as_str(), "in_review" | "merge_failed") {
        return false;
    }
    // RAL-250: when this review has opted out of base-branch auto-updates, the
    // maintenance sweep must not rebuild it on a base shift. This gates ONLY
    // this automatic pass -- a manual `review upstream set` / merge goes
    // through its own explicit path and is unaffected.
    if guardian.effective_skip_base_updates {
        return false;
    }

    // Check every project in the guardian for a base-branch shift. Also track
    // (RAL-300) whether every project's own enabled branches are already an
    // ancestor of that project's *current* base -- the inverse direction from
    // a shift: not "the base moved past us" but "the base already contains
    // us" (e.g. a fast-forward merge that landed the stack's work
    // outside any tracked PR). `fully_landed` starts true and is cleared by
    // any project this can't positively confirm for, so an unresolvable
    // project never silently counts as "safe to skip".
    let mut any_shifted = false;
    let mut all_have_baseline = true;
    let mut fully_landed = !guardian.projects.is_empty();
    let mut shift_detail: Vec<String> = Vec::new();
    for proj in &guardian.projects {
        let root = Workspace::for_guardian(store, id, Path::new(proj));
        let current = match resolve_base(&root, &guardian.base_branch) {
            Ok(s) => s,
            Err(_) => {
                fully_landed = false;
                continue; // branch gone/unresolvable: skip this project
            }
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
            Some(prev) => {
                any_shifted = true;
                shift_detail.push(format!(
                    "{proj} ({}): {}..{}",
                    guardian.base_branch,
                    &prev[..prev.len().min(8)],
                    &current[..current.len().min(8)]
                ));
            }
        }
        if !project_already_in_base(&root, &guardian, proj, &current) {
            fully_landed = false;
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
    // RAL-300: a base shift alone doesn't mean this review has new upstream
    // work to rebase onto -- if the shift itself is every project absorbing
    // this review's own branches (already-ancestor for all of them), then the
    // shift IS this review landing, not something to rebuild against.
    // Approve outright instead of wasting a rebuild on a base that already
    // has us; only from `in_review` -- `approve_guardian` has no other
    // transition, and a `merge_failed` review still needs a human regardless.
    if guardian.status.as_str() == "in_review"
        && fully_landed
        && approve_base_already_landed(store, id)
    {
        return true;
    }
    // Claim the review under one lock (flip to Merging) so a concurrent
    // maintenance pass cannot also start rebuilding it.
    let detail = format!(
        "base branch changed; rebuilding ({})",
        shift_detail.join(", ")
    );
    let claimed = {
        let g = store.lock().expect("poisoned");
        matches!(g.get_guardian(id), Ok(gv) if matches!(gv.status.as_str(), "in_review" | "merge_failed"))
            && g.set_guardian_status(id, GuardianStatus::Merging, Some(&detail))
                .is_ok()
    };
    if claimed {
        let _permit = sem.acquire();
        run_merge_staged(store, runner, id, cancel);
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
    branch_id: &str,
    feature_branch: &str,
    base_sha: &str,
    newbase: &str,
    rev: &str,
    wt: &Workspace,
    squash: bool,
    gate: &ProofGate,
    cancel: &CancelToken,
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
        branch_id,
        runner,
        feature_branch,
        wt,
        newbase,
        base_sha,
        rev,
        gate,
        cancel,
    ) {
        Ok((outcome, session_id)) => {
            let (status, detail): (MergeStatus, Option<String>) = match outcome {
                RebaseOutcome::Resolved(note) => (MergeStatus::ConflictResolved, Some(note)),
                // RAL-168: proofed but no conflict occurred -- still `Done`.
                RebaseOutcome::CleanProofed(note) => (MergeStatus::Done, Some(note)),
                RebaseOutcome::Clean => (
                    MergeStatus::Done,
                    // Surface a branch that added nothing over the base rather than
                    // reporting a silent, work-free "done".
                    contributed_nothing(wt, newbase, rev)
                        .then(|| "no new commits over base (already merged?)".to_string()),
                ),
            };
            promote_branch_terminal(
                store,
                runner,
                id,
                branch_id,
                status,
                detail.as_deref(),
                session_id.as_deref(),
            );
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
            // RAL-213: a cancelled merge is already logged by `drive_rebase`'s
            // own checkpoint -- never mark the branch/guardian failed for it.
            if cancel.is_cancelled() {
                return Err(());
            }
            fail_branch(store, id, branch_id, feature_branch, &e, &set_status);
            Err(())
        }
    }
}

/// Rebuild the combined review worktree at `prev_ref` and run the *deterministic*
/// check gates (see [`final_checks`] for the full RAL-342 precedence).
///
/// Returns an optional informational note for the `InReview` status: `Some(...)`
/// when the check gates were opted out (CCTL-130), a review-declared auto_build
/// (RAL-342) ran, or a config auto-build ran in place of explicit checks
/// (RAL-101), so the UI can distinguish those from a plain "checks passed";
/// `None` when explicit checks ran and passed, or nothing ran at all (no checks,
/// no review/project auto_build configured).
fn finalize_review(
    store: &Arc<Mutex<Store>>,
    runner: &dyn Runner,
    root: &Workspace,
    wt_base: &Workspace,
    id: &str,
    prev_ref: &str,
    cancel: &CancelToken,
) -> std::result::Result<Option<String>, String> {
    let combined_str = rebuild_combined(store, root, wt_base, id, prev_ref)?;
    final_checks(store, runner, id, root, &combined_str, cancel)
}

/// Run the review's check gates against the finished combined worktree.
///
/// Four-tier precedence (RAL-342, replacing the old RAL-110 AI-guessed build):
/// skip (opt-out) → this review's own explicit `checks` gates → this review's
/// declared `[[review.auto_build]]` step → the project's `.ralphus.toml`
/// `auto_build` default → nothing. Only one tier ever runs.
///
/// Returns `Some(note)` when checks were opted out, a review auto_build ran, or
/// the config auto-build (local or remote) ran (so the UI can show what
/// happened), `None` when explicit checks ran and passed or nothing ran at all,
/// or `Err` on an explicit check gate failure. Per RAL-342 Q5, a failed
/// review-declared auto_build is deliberately NOT an `Err` -- see the inline
/// comment on that tier below.
fn final_checks(
    store: &Arc<Mutex<Store>>,
    runner: &dyn Runner,
    id: &str,
    root: &Workspace,
    combined_str: &str,
    cancel: &CancelToken,
) -> std::result::Result<Option<String>, String> {
    let (skip_auto_build, checks, auto_build, env) = {
        let guard = store.lock().expect("poisoned");
        (
            guard.guardian_skip_auto_build(id).unwrap_or(false),
            guard.guardian_checks(id).unwrap_or_default(),
            guard.guardian_auto_build(id).unwrap_or_default(),
            // RAL-203: run under the same environment the combined worktree's
            // branches were built with (`combined_env`), plus this review's
            // own build-step overrides -- a check gate like `cargo test` is
            // worthless if it runs without the variables the code expects.
            guard
                .get_guardian(id)
                .map(|g| g.build_env)
                .unwrap_or_default(),
        )
    };

    if skip_auto_build {
        return Ok((!checks.is_empty()).then(|| "check gates skipped (opt-out)".to_string()));
    }
    if !checks.is_empty() {
        for cmd in &checks {
            // RAL-239: same reasoning as `run_commit_checks` -- don't start the
            // next final check gate once the review has been cancelled.
            if cancel.is_cancelled() {
                return Err("cancelled".to_string());
            }
            if !root
                .at(combined_str)
                .run_command_with_env(cmd, &env, cancel)
                .0
            {
                return Err(format!("check failed: {cmd}"));
            }
        }
        return Ok(None);
    }
    // RAL-342: no explicit checks -- try this review's own declared
    // `[[review.auto_build]]` step next, ahead of the project-wide default.
    // Per Q5, a failure here is advisory only (a UI notice + Cartographer log)
    // rather than a merge-failing `Err` -- unlike explicit `checks`, which the
    // user wrote as a hard gate, an auto_build declaration is a convenience
    // build step and should never block a review from reaching `InReview`.
    if let Some(def) = auto_build {
        if let Some(note) =
            run_review_auto_build(store, runner, id, root, combined_str, &env, &def, cancel)
        {
            return Ok(Some(note));
        }
    }
    // RAL-101: no explicit checks or review auto_build -- fall back to the
    // project's default build/test command, if one is configured, so
    // "in review" still means "testable" rather than "merged and never built".
    match crate::config::resolve(root.root()).auto_build {
        Some(cmd) => {
            if !root
                .at(combined_str)
                .run_command_with_env(&cmd, &env, cancel)
                .0
            {
                return Err(format!("auto-build failed: {cmd}"));
            }
            Ok(Some(format!("auto-built via project default: {cmd}")))
        }
        None => Ok(None),
    }
}

/// Run this review's declared `[[review.auto_build]]` step (RAL-342) against the
/// finished combined worktree -- either a static shell `command`, or an agent
/// invocation described by `def`'s remaining fields (exactly one shape is
/// populated, enforced by `core::validate` at parse time).
///
/// Returns `Some(note)` when the step ran, whether it succeeded or failed --
/// per Q5 a failure is surfaced as an advisory [`Store::set_guardian_notice`]
/// plus a Cartographer log entry, never as an `Err`, so the caller always
/// treats this tier as "handled" once it fires and falls through to `InReview`
/// exactly as if this tier had been absent. `None` only when `def` describes
/// no runnable step (defensive; `core::validate` should never allow this).
#[allow(clippy::too_many_arguments)]
fn run_review_auto_build(
    store: &Arc<Mutex<Store>>,
    runner: &dyn Runner,
    id: &str,
    root: &Workspace,
    combined_str: &str,
    env: &std::collections::BTreeMap<String, String>,
    def: &crate::guardian::GuardianAutoBuild,
    cancel: &CancelToken,
) -> Option<String> {
    if let Some(cmd) = def.command.as_deref() {
        let ok = root
            .at(combined_str)
            .run_command_with_env(cmd, env, cancel)
            .0;
        let _ = store.lock().expect("poisoned").cartographer_log(
            crate::cartographer::CartographerEntry {
                level: if ok {
                    crate::logging::LogLevel::INFO
                } else {
                    crate::logging::LogLevel::WARNING
                },
                source: "guardian",
                message: if ok {
                    "review auto_build succeeded"
                } else {
                    "review auto_build failed"
                },
                scope: Some("guardian"),
                squad_id: None,
                guardian_id: Some(id),
                cell_id: None,
                task: None,
                log_path: None,
                payload: serde_json::json!({"command": cmd}),
                admin_only: false,
            },
        );
        if !ok {
            let _ = store.lock().expect("poisoned").set_guardian_notice(
                id,
                "auto_build_failed",
                &format!(
                    "This review's declared auto_build command failed: {cmd}. The review has \
                     still moved to In Review -- check the build output and re-run manually if \
                     needed."
                ),
            );
        }
        return Some(format!(
            "auto-built via review auto_build: {cmd}{}",
            if ok { "" } else { " (failed)" }
        ));
    }

    // Agent-invocation shape: `def.prompt` (plus optional system prompt/agent/model).
    let prompt = def.prompt.as_deref()?;
    let cwd = combined_str.to_string();
    let (stored_agent, stored_model) = {
        let guard = store.lock().expect("poisoned");
        let g = guard.get_guardian(id).ok();
        (
            g.as_ref().and_then(|g| g.resolver_agent.clone()),
            g.and_then(|g| g.resolver_model.clone()),
        )
    };
    let resolved = match resolve_resolver_agent(
        def.agent.as_deref().or(stored_agent.as_deref()),
        def.model.as_deref().or(stored_model.as_deref()),
        Path::new(&cwd),
    ) {
        Ok(r) => r,
        Err(message) => {
            let _ = store.lock().expect("poisoned").set_guardian_notice(
                id,
                "auto_build_failed",
                &format!(
                    "This review's declared auto_build agent could not be resolved: {message}. \
                     The review has still moved to In Review."
                ),
            );
            return Some(format!("auto-build agent unresolvable: {message}"));
        }
    };
    let spec = RunnerSpec {
        squad_id: format!("guardian-{id}"),
        task: AUTO_BUILD_TASK.to_string(),
        cell_id: AUTO_BUILD_SESSION.to_string(),
        cwd,
        prompt: Some(prompt.to_string()),
        command: None,
        agent: resolved.backend.clone(),
        executable: resolved.executable.clone(),
        model: resolved.model.clone(),
        system_prompt: def.system_prompt.clone(),
        system_prompt_position: def.system_prompt_position.clone(),
        timeout_sec: None,
        budget_tokens: None,
        maximum_budget_usd: None,
        maximum_context: None,
        auto_compact_threshold: None,
        maximum_tool_output_tokens: None,
        proof: false,
        trace_context: None,
        resume_agent_session_id: None,
        assigned_agent_session_id: None,
        env_overrides: {
            let mut e = resolved.env.clone();
            e.extend(env.clone());
            e
        },
        machine: root.machine().map(str::to_string),
        tool_arg_truncate_chars: None,
        thrash_max_compactions: None,
        thrash_min_turn_gap: None,
        allow_personal_settings: false,
        allow_personal_memory: false,
    };
    let result = runner.run_cancellable(&spec, cancel);
    let _ = record_guardian_call_cost(store, id, None, "auto_build", &result);
    let ok = result.is_done();
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
                    "review auto_build (agent) succeeded"
                } else {
                    "review auto_build (agent) failed"
                },
                scope: Some("guardian"),
                squad_id: None,
                guardian_id: Some(id),
                cell_id: None,
                task: None,
                log_path: None,
                payload: serde_json::json!({"summary": result.summary}),
                admin_only: false,
            });
    if !ok {
        let _ = store.lock().expect("poisoned").set_guardian_notice(
            id,
            "auto_build_failed",
            &format!(
                "This review's declared auto_build agent invocation failed: {}. The review has \
                 still moved to In Review -- check the agent output and re-run manually if \
                 needed.",
                result.error.as_deref().unwrap_or("no summary produced")
            ),
        );
    }
    Some(if ok {
        "auto-built via review auto_build (agent)".to_string()
    } else {
        "auto-build (agent) failed".to_string()
    })
}

/// Fetch a branch produced on another machine into this repository, so the
/// review can stack it (RAL-185 Phase 3b).
///
/// Per **D2** the daemon never *publishes* — deciding what to commit is
/// judgment, and a generic `add -A && commit && push` would sweep up build
/// artifacts and contradict the per-cell control task files already
/// exercise. The task's own cell is responsible for pushing. Fetching a
/// branch whose name and remote are both already known is the opposite: fully
/// deterministic, so it belongs here.
///
/// A no-op for a locally-produced branch, which is every pre-RAL-185 branch.
///
/// **This is the check that catches a task that never pushed.** Without it the
/// review would either fail deep inside `worktree add` with an opaque "invalid
/// reference" message, or — worse, when a *previous* squad did push — quietly
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
fn fetch_branch_for_remote_cell(
    store: &Arc<Mutex<Store>>,
    guardian_id: &str,
    branch: &crate::guardian::BranchView,
) -> std::result::Result<(), String> {
    let Some(machine) = branch
        .source_cell_machine
        .as_deref()
        .map(str::trim)
        .filter(|m| !m.is_empty() && !m.eq_ignore_ascii_case(ralphus_core::schema::LOCAL_MACHINE))
    else {
        return Ok(());
    };
    let review = store
        .lock()
        .expect("poisoned")
        .get_guardian(guardian_id)
        .ok();
    let root = branch
        .project
        .clone()
        .or_else(|| review.as_ref().map(|g| g.git_root.clone()))
        .ok_or_else(|| format!("branch {} has no project root to fetch into", branch.branch))?;
    let root = Path::new(&root);
    // Same remote the review's PRs resolve against (RAL-282), keyed off its own
    // base branch -- fetching a remote-produced branch from a different remote
    // than the one the review targets is how a stale/absent branch slips through.
    let base_branch = review
        .as_ref()
        .map(|g| g.base_branch.as_str())
        .unwrap_or_default();
    let remote =
        crate::forge::resolve_remote_name(root, base_branch, &crate::config::resolve_forge(root));
    let vcs = {
        let guard = store.lock().expect("poisoned");
        crate::vcs::for_project_root(&guard, root)?
    };

    if let Err(e) = vcs.fetch_branch(root, &remote, &branch.branch) {
        return Err(format!(
            "branch \"{}\" was produced on machine \"{machine}\" but could not be fetched from \
             \"{remote}\": {e}. The task that owns this branch is responsible for pushing it \
             before it completes — ralphus never commits or pushes on a cell's behalf. Check \
             that cell's output, confirm it pushed, then restart this review.",
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
/// task whose cell never committed produces a branch identical to its base,
/// which rebases perfectly and merges perfectly, so the review reaches
/// `in_review` looking entirely healthy while containing none of that task's
/// work. Nothing else in the pipeline notices — proof steps check the *code*,
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
/// A *task* is allowed to produce no changes (a read-only analysis cell, a
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
    let combined_branch = claim_combined_review_ref_by_id(store, root, id)?;
    let combined_wt = wt_base.join("review");
    let combined_str = combined_wt.root().to_string_lossy().to_string();
    worktree_add_or_reset(root, &combined_branch, &combined_wt, prev_ref)?;
    {
        let guard = store.lock().expect("poisoned");
        let _ = guard.set_guardian_review_branch(id, &combined_branch);
        let _ = guard.set_guardian_combined_worktree(id, &combined_str);
    }
    regenerate_readme(store, root);
    Ok(combined_str)
}

/// Regenerate `.git/.ralphus/README.md` for the project at `root`, listing
/// every currently-registered guardian's short-name -> branch mapping so a
/// human who opens the folder can tell what e.g. `g/g56/wt-RAL-121` refers
/// to. Called after every successful merge/rebuild.
///
/// Best-effort: a write failure here must never fail the review itself, so
/// errors are swallowed rather than propagated.
fn regenerate_readme(store: &Arc<Mutex<Store>>, root: &Workspace) {
    let guardians = store
        .lock()
        .expect("poisoned")
        .list_guardians()
        .unwrap_or_default();
    let mut mappings: Vec<(String, String)> = Vec::new();
    for gv in guardians
        .iter()
        .filter(|gv| Path::new(&gv.git_root) == root.root())
    {
        let short_id = crate::short_paths::guardian_short_id(&gv.id);
        for b in &gv.branches {
            let Some(wt) = &b.worktree else { continue };
            let Some(name) = Path::new(wt).file_name() else {
                continue;
            };
            mappings.push((
                format!("g/{short_id}/{}", name.to_string_lossy()),
                b.branch.clone(),
            ));
        }
        if gv.combined_worktree.is_some() {
            mappings.push((
                format!("g/{short_id}/review"),
                format!("combined review for guardian \"{}\"", gv.name),
            ));
        }
    }
    mappings.sort();
    let readme = crate::short_paths::render_readme(&mappings);
    let _ = root.write_file(
        Path::new(".git").join(".ralphus").join("README.md"),
        &readme,
    );
}

/// The extra characters a per-branch or combined review worktree directory
/// name can add under a guardian's `wt_base` -- `"wt-"` (3) plus a
/// 12-character truncated branch name, plus headroom for a `-99`-style
/// collision suffix. Used as a conservative stand-in for the real worktree
/// path in [`preflight_worktree_budget`], which runs once per project build
/// rather than once per branch: every review branch in a project starts from
/// the same snapshotted base tree, so one measurement of it (per the RAL-211
/// ticket's own guidance) covers the whole stack before any review branch
/// worktree exists.
const WORST_CASE_WT_SUFFIX_LEN: usize = 3 + 12 + 3; // "wt-" + 12 chars + "-99"

/// OS path-length budget a worktree checkout is held to -- mirrors
/// `crate::worktrees::path_budget_limit`'s identical rationale (Windows'
/// `MAX_PATH` is 260 characters; other platforms' limits are high enough in
/// practice that enforcing this there too would only produce false failures).
fn path_budget_limit() -> usize {
    if cfg!(windows) { 260 } else { usize::MAX }
}

/// Preflight (RAL-211): before this project's branches start materializing
/// worktrees under `wt_base`, measure whether the deepest tracked path in
/// `git_ref` (the snapshotted base) would overflow [`path_budget_limit`] once
/// combined with `wt_base` plus a worst-case per-branch directory name, and
/// fail with the arithmetic spelled out rather than letting a `git worktree
/// add` deep in the stack fail with an opaque `Filename too long`.
///
/// Reads the tree from the object database (`git ls-tree`, not `ls-files`) --
/// no checkout, no network.
fn preflight_worktree_budget(
    root: &Workspace,
    wt_base: &Workspace,
    git_ref: &str,
) -> Result<(), String> {
    preflight_worktree_budget_with_limit(root, wt_base, git_ref, path_budget_limit())
}

/// [`preflight_worktree_budget`] with the limit taken as a plain parameter
/// rather than read from [`path_budget_limit`], so it is testable without
/// depending on the host OS.
fn preflight_worktree_budget_with_limit(
    root: &Workspace,
    wt_base: &Workspace,
    git_ref: &str,
    limit: usize,
) -> Result<(), String> {
    if limit == usize::MAX {
        return Ok(());
    }
    let listing = root
        .git(&["ls-tree", "-r", "-z", "--name-only", git_ref])
        .map_err(|e| {
            format!("could not measure \"{git_ref}\" for a worktree path preflight check: {e}")
        })?;
    let worst_case = wt_base.root().join("x".repeat(WORST_CASE_WT_SUFFIX_LEN));
    crate::short_paths::check_worktree_path_budget(&worst_case, &listing, limit)
}

/// Remove every review worktree/branch this guardian created previously, so a
/// re-merge starts from a clean slate. Worktrees are matched against
/// `wt_base` (the guardian's own `.git/.ralphus/g/g<n>` directory) by path
/// component, not substring -- see [`crate::short_paths::worktree_belongs_to_guardian`]
/// for why a substring match on a short id is unsafe (`g56` would match a
/// worktree actually belonging to `g560`). Branches are removed via
/// `for-each-ref` covering the internal naming (`guardian/<id>/*`) and the
/// legacy `guardian/<num>/*` scheme for reviews built before RAL-63.
///
/// RAL-378: a readable review branch (`<task branch>-review`) matches no glob
/// -- by design, since a glob over the user's own branch namespace would
/// delete branches this review never created. Those are removed by exact name
/// instead, via `claimed_branches`, which callers fill from the review's
/// persisted `review_branch_name` columns.
fn cleanup_review_worktrees(
    root: &Workspace,
    wt_base: &Workspace,
    id: &str,
    num: &str,
    claimed_branches: &[String],
) {
    let list = root
        .git(&["worktree", "list", "--porcelain"])
        .unwrap_or_default();
    for line in list.lines() {
        if let Some(path) = line.strip_prefix("worktree ") {
            let path = path.trim();
            if crate::short_paths::worktree_belongs_to_guardian(path, wt_base.root(), id) {
                // Unlock first so that a locked worktree does not block removal.
                let _ = root.git(&["worktree", "unlock", path]);
                // Two --force flags handle dirty/untracked (first) and locked (second).
                let _ = root.git(&["worktree", "remove", "--force", "--force", path]);
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
    for branch in claimed_branches {
        let _ = root.git(&["branch", "--delete", "--force", branch]);
    }
    // RAL-201: was `git(root.root(), ...)`, a direct bypass of `root`'s
    // machine sitting right next to the correctly-routed calls in this same
    // function.
    let refs = root
        .git(&[
            "for-each-ref",
            "--format=%(refname:short)",
            // Current naming: guardian/<id>/wt-* and guardian/<id>/review.
            &format!("refs/heads/guardian/{id}"),
            &format!("refs/heads/guardian/{id}/*"),
            // Legacy pre-RAL-63 naming: guardian/<num>/b*.
            &format!("refs/heads/guardian/{num}"),
            &format!("refs/heads/guardian/{num}/*"),
            // RAL-378: the interim `refs/heads/review` / `refs/heads/review-*`
            // patterns this used to also sweep are gone. They date from a
            // naming scheme no review has used since before RAL-63, they are
            // not scoped to this guardian at all, and they sit in the same
            // unprefixed namespace readable review branches now occupy -- so
            // the only branches they can still match are a user's own.
        ])
        .unwrap_or_default();
    for branch in refs.lines().map(str::trim).filter(|b| !b.is_empty()) {
        let _ = root.git(&["branch", "--delete", "--force", branch]);
    }
}

/// Review worktrees with no activity for thirty days are stale. This is long
/// enough to avoid surprising an ordinary review cycle while still bounding
/// accumulation in a daemon that runs for months.
#[cfg(any())]
pub(crate) const WORKTREE_RETIREMENT_AGE_MS: i64 = 30 * 24 * 60 * 60 * 1_000;

#[cfg(any())]
fn normalized_worktree_path(path: &Path) -> String {
    let resolved = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    let text = resolved.to_string_lossy();
    let text = text.strip_prefix(r"\\?\").unwrap_or(&text);
    text.replace('\\', "/")
        .trim_end_matches('/')
        .to_ascii_lowercase()
}

#[cfg(any())]
fn terminal_worktree_claim(kind: &str, state: &str) -> bool {
    match kind {
        "cell" | "proof" => matches!(state, "done" | "cancelled" | "failed"),
        // Review lifecycle names differ from NodeState: deployed is its done
        // state and merge_failed is its failed state.
        "review" => matches!(state, "deployed" | "cancelled" | "merge_failed"),
        _ => false,
    }
}

/// Retire old guardian worktrees whose every persisted Cell, Proof, and Review
/// claim is terminal. Old worktrees with a non-terminal claim are retained and
/// raise one durable, per-path mailbox escalation instead.
///
/// Called only from the scheduler's daily interval. Git and filesystem work
/// happen without holding the store mutex; each short snapshot/update does.
#[cfg(any())]
pub fn retire_stale_worktrees(store: &Arc<Mutex<Store>>) {
    let (records, claims) = {
        let guard = store.lock().expect("poisoned");
        let records = match guard.guardian_worktree_records() {
            Ok(records) => records,
            Err(error) => {
                // ralphus[ignore-rlog-pair]: transient snapshot read diagnostic; actual retirement emits its structured outcome
                crate::rlog!(
                    WARNING,
                    "ralphus [guardian] worktree retirement snapshot failed: {error}"
                );
                return;
            }
        };
        let claims = match guard.worktree_claims() {
            Ok(claims) => claims,
            Err(error) => {
                crate::rlog!(
                    WARNING,
                    "ralphus [guardian] worktree claim snapshot failed: {error}"
                );
                return;
            }
        };
        (records, claims)
    };
    let cutoff = crate::store::now_ms().saturating_sub(WORKTREE_RETIREMENT_AGE_MS);
    // A combined worktree is also the last branch's worktree, so it commonly
    // has two rows. Age is the newest activity across every row for that path.
    let mut records_by_path = HashMap::new();
    for record in records {
        let key = normalized_worktree_path(Path::new(&record.path));
        let replace =
            records_by_path
                .get(&key)
                .is_none_or(|old: &crate::store::GuardianWorktreeRecord| {
                    record.last_activity_ms > old.last_activity_ms
                });
        if replace {
            records_by_path.insert(key, record);
        }
    }
    for (key, record) in records_by_path {
        if record.last_activity_ms > cutoff {
            continue;
        }
        let root = Workspace::for_guardian(
            store,
            &record.guardian_id,
            PathBuf::from(&record.project_root),
        );
        let wt_base = root.at(worktree_dir(&record.project_root, &record.guardian_id));
        if !crate::short_paths::worktree_belongs_to_guardian(
            &record.path,
            wt_base.root(),
            &record.guardian_id,
        ) {
            continue;
        }
        let registered = root
            .git(&["worktree", "list", "--porcelain"])
            .unwrap_or_default()
            .lines()
            .filter_map(|line| line.strip_prefix("worktree "))
            .any(|path| normalized_worktree_path(Path::new(path.trim())) == key);
        if !registered {
            continue;
        }
        let active_claim = claims.iter().find(|claim| {
            normalized_worktree_path(Path::new(&claim.path)) == key
                && !terminal_worktree_claim(&claim.kind, &claim.state)
        });
        if let Some(claim) = active_claim {
            let guard = store.lock().expect("poisoned");
            if guard
                .claim_ark_notification("guardian-worktree", &key)
                .unwrap_or(false)
            {
                let message = format!(
                    "Guardian worktree {} for review {} ({}) is over 30 days old but is still claimed by a non-terminal {} ({}). If this Task or Review is no longer needed, please cancel it.",
                    record.path, record.guardian_id, record.guardian_name, claim.kind, claim.owner
                );
                let entity_uri = format!("guardian:{}", record.guardian_id);
                let _ = guard.enqueue_mailbox_message(
                    crate::mailbox::MailboxPriority::High,
                    &message,
                    None,
                    None,
                    None,
                    Some(&entity_uri),
                );
                crate::cartographer::Note::new("guardian")
                    .guardian(&record.guardian_id)
                    .scope("guardian")
                    .emit(
                        &guard,
                        "old guardian worktree retained because it is still claimed",
                        serde_json::json!({"worktree": record.path, "claim_kind": claim.kind, "claim_owner": claim.owner}),
                    );
            }
            continue;
        }
        let _ = root.git(&["worktree", "unlock", &record.path]);
        match root.git(&["worktree", "remove", "--force", "--force", &record.path]) {
            Ok(_) => {
                let guard = store.lock().expect("poisoned");
                let _ = guard.clear_guardian_worktree_path(&record.path);
                crate::cartographer::Note::new("guardian")
                    .guardian(&record.guardian_id)
                    .scope("guardian")
                    .emit(
                        &guard,
                        "retired old guardian worktree",
                        serde_json::json!({"worktree": record.path, "age_threshold_days": 30}),
                    );
            }
            Err(error) => crate::rlog!(
                WARNING,
                "ralphus [guardian] could not retire worktree {}: {error}",
                record.path
            ),
        }
    }
}

/// RAL-317: record a branch's terminal (`Done`/`ConflictResolved`) merge
/// outcome and, if the review's effective `auto_submit_pr_stack` setting is
/// on, auto-submit/grow its PR stack to include the newly-terminal branch.
/// Shared by every site that can move a branch to a terminal status --
/// `stack_pick` (the real per-branch stacked-rebase path `run_merge_staged`
/// drives) and `run_merge_shared`'s legacy/shared-worktree fallback -- so the
/// auto-submit hook is written once rather than duplicated per call site. The
/// PR-stack side effect (`crate::pr::maybe_auto_submit_branch`) is a
/// best-effort side channel: it never fails or blocks this transition, and
/// any failure is recorded per-branch (`BranchView::auto_submit_error`)
/// rather than surfaced here.
fn promote_branch_terminal(
    store: &Arc<Mutex<Store>>,
    runner: &dyn Runner,
    id: &str,
    branch_id: &str,
    status: MergeStatus,
    detail: Option<&str>,
    session_id: Option<&str>,
) {
    {
        let guard = store.lock().expect("poisoned");
        let _ = guard.set_branch_status(id, branch_id, status, detail);
        if let Some(sid) = session_id {
            let _ = guard.set_branch_resolver_session_id(id, branch_id, sid);
        }
    }
    crate::pr::maybe_auto_submit_branch(store, runner, id, branch_id);
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
    /// Rebased with no conflicts, and no dedicated proof call ran (a true
    /// no-op, or Proof scope skipped it -- RAL-168).
    Clean,
    /// Rebased with no conflicts, but a dedicated proof call ran anyway
    /// (RAL-168 "each_branch"/"final_branch" scope, on a branch that
    /// contributed real changes). Carries the branch detail message to
    /// record, same shape as [`RebaseOutcome::Resolved`]'s -- still reported
    /// as `Done`, not `ConflictResolved`, since no conflict actually occurred.
    CleanProofed(String),
    /// Rebased after the agent resolved conflicts (and, per Proof scope,
    /// possibly ran the RAL-149 final-proof call). Carries the branch
    /// detail message to record (e.g. "resolved by agent; final proof
    /// passed/failed: ...", or "...skipped (Proof scope)").
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
    branch_id: &str,
    runner: &dyn Runner,
    feature: &str,
    wt: &Workspace,
    newbase: &str,
    base_sha: &str,
    branch_arg: &str,
    gate: &ProofGate,
    cancel: &CancelToken,
) -> std::result::Result<(RebaseOutcome, Option<String>), String> {
    if cancel.is_cancelled() {
        log_merge_cancelled(store, id);
        return Err("cancelled".to_string());
    }
    // RAL-259: this `drive_rebase` call is one branch-resolution attempt, so
    // clear the branch's Live-View start time before anything runs — a
    // re-merge / restart re-stamps from scratch (see
    // `Store::stamp_branch_started_at`'s COALESCE, fired at each actual
    // resolver/proof session start below). Left NULL if no resolver ever runs
    // for this branch.
    let _ = store
        .lock()
        .expect("poisoned")
        .clear_branch_started_at(id, branch_id);
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
    let mut result = wt.git(&args);
    if matches!(&result, Err(e) if e.contains("untracked working tree files would be overwritten"))
    {
        // `git rebase --onto <newbase>` fails with exactly this message when the
        // worktree contains a file that is tracked in `newbase` but untracked
        // here -- a common leftover from a prior agent cell that didn't stage
        // everything, especially likely on Windows where a CWD lock prevents
        // `worktree_add_or_reset` from deleting and recreating the directory
        // cleanly. Only reach for `git clean -fd` -- which wipes every
        // untracked file, not just the blocking one -- as a retry after this
        // specific failure, not unconditionally before every attempt: an
        // unrelated untracked file (a still-in-progress build artifact, a
        // resumable worktree's own bookkeeping) has no business being swept
        // away by a rebase that would have succeeded without it.
        let _ = wt.git(&["rebase", "--abort"]);
        let _ = wt.git(&["clean", "--force", "-d"]);
        result = wt.git(&args);
    }
    match result {
        Ok(_) => {
            // RAL-168: "each_branch" (without auto-clean-skip) or
            // "final_branch" (on the last branch) also proves a branch that
            // rebased cleanly -- not just one whose conflicts the agent
            // resolved -- as long as it actually contributed real changes (a
            // true no-op always skips the proof call, no setting needed).
            let nothing = contributed_nothing(wt, newbase, branch_arg);
            if nothing || !gate.allows_for_clean_branch() {
                return Ok((RebaseOutcome::Clean, None));
            }
            let resolved = resolver_backend(store, id)?;
            let (quality_note, ghost_prefix) =
                proof_extras(store, id, feature, branch_id, runner, &resolved, cancel);
            let (proof_session_id, proof_detail) = run_final_proof(
                store,
                id,
                branch_id,
                runner,
                wt,
                feature,
                &resolved,
                &quality_note,
                &ghost_prefix,
                cancel,
            );
            Ok((RebaseOutcome::CleanProofed(proof_detail), proof_session_id))
        }
        Err(e) => {
            if cancel.is_cancelled() {
                log_merge_cancelled(store, id);
                return Err("cancelled".to_string());
            }
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
                        squad_id: None,
                        guardian_id: Some(id),
                        cell_id: None,
                        task: None,
                        log_path: None,
                        payload: serde_json::json!({"branch": feature}),
                        admin_only: false,
                    });
                }
                let resolved = resolver_backend(store, id)?;
                match resolve_conflicts_with_agent(
                    store, id, branch_id, runner, wt, feature, &resolved, gate, cancel,
                ) {
                    Ok((session_id, proof_detail)) => {
                        Ok((RebaseOutcome::Resolved(proof_detail), session_id))
                    }
                    Err(re) => {
                        if cancel.is_cancelled() {
                            // Already logged by `resolve_conflicts_with_agent`'s own
                            // checkpoint -- just propagate without a destructive abort.
                            return Err(re);
                        }
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
    if let Err(e) = wt.git(&["commit", "--no-verify", "--message", &msg]) {
        // Restore the pre-squash tip so the stack is not left in a dirty state.
        let _ = wt.git(&["reset", "--soft", "ORIG_HEAD"]);
        return Err(e);
    }
    // ralphus[ignore-rlog-pair]: this low-level helper has no Store; its Store-owning caller records the structured workflow outcome
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

/// RAL-103: Recompute a preliminary, git-log-only change summary covering
/// EVERY enabled branch that has left `pending`. Unlike
/// [`generate_final_summary`], this never calls an LLM, so it is cheap enough
/// to recompute every time another branch reaches `Ready`, while the guardian
/// is still `collecting`.
///
/// Each branch contributes exactly one section, read from whichever ref
/// actually holds its commits: its review ref (`guardian/<id>/wt-<branch>`)
/// once its stacked rebase has run, otherwise its producing task cell's own
/// worktree HEAD. RAL-303: this used to look at *only* the branches with no
/// review worktree, which meant that as soon as part of the stack had been
/// rebased, a recompute rewrote the whole summary down to just the branches
/// that hadn't — a seven-branch review whose newest branch was the only
/// un-rebased one ended up with a change summary describing that one branch.
/// A branch this cannot read at all (no review ref and no known producing
/// worktree) contributes nothing rather than being guessed at.
///
/// Every commit subject appears exactly once across the whole summary,
/// attributed to the earliest branch in stack order that carries it. The
/// `prev..next` ranges below already keep a stacked branch from re-listing the
/// commits of the branch beneath it, but that only holds *within* one chain:
/// a review ref is a rebased copy of the producing worktree's commits, so the
/// same commit has one sha on the review side and a different sha on the task
/// side, and a partly-rebased stack reads from both. RAL-303: the subject-level
/// pass below is what actually guarantees uniqueness across that seam.
///
/// A no-op (leaves `change_summary` untouched) when no qualifying branch has
/// any commits to show — that "nothing ready yet" state is instead surfaced
/// by `summary_state == "waiting"` — and likewise when every enabled branch
/// has already been rebased and an LLM-authored final summary exists, since
/// there is nothing this pass can add that [`generate_final_summary`] hasn't
/// already said better.
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
        /// The ref this branch's stacked review commits live on -- resolved
        /// once here, where the owning `BranchView` (and so its claimed
        /// readable name) is still in hand.
        review_ref: String,
        project: String,
        /// The producing task cell's worktree, when the store still knows one.
        cwd: Option<String>,
        // RAL-201: the *producing task cell's* machine, not the review's --
        // reading a branch's own task worktree per RAL-185 D3/D4 may mean a
        // different machine than the review this guardian will eventually run
        // on, or no machine at all when the review itself is remote but this
        // task ran locally.
        machine: Option<String>,
        /// Whether the stacked rebase has already produced a review ref.
        rebased: bool,
        /// Whether this branch contributes a section at all. A disabled branch
        /// is carried here purely to advance the task-side base past it (see
        /// the loop below), never to be reported.
        reported: bool,
    }
    let (git_root, base_branch, base_commits, has_final_summary, candidates) = {
        let guard = store.lock().expect("store mutex poisoned");
        let Ok(guardian) = guard.get_guardian(id) else {
            return;
        };
        let candidates = guardian
            .branches
            .iter()
            // A disabled branch is kept (see `reported`) but a `pending` one is
            // dropped outright -- it has no commits anywhere yet to skip past.
            .filter(|b| !b.enabled || b.merge_status != "pending")
            .map(|b| Candidate {
                branch: b.branch.clone(),
                review_ref: review_ref_of(&guardian.id, b),
                project: b
                    .project
                    .clone()
                    .unwrap_or_else(|| guardian.git_root.clone()),
                cwd: guard.cell_cwd_for_branch(&b.branch).ok().flatten(),
                machine: b.source_cell_machine.clone(),
                rebased: b.worktree.is_some(),
                reported: b.enabled,
            })
            .collect::<Vec<_>>();
        (
            guardian.git_root,
            guardian.base_branch,
            guardian.base_commits,
            guardian.summary_agent.is_some(),
            candidates,
        )
    };
    if !candidates.iter().any(|c| c.reported) {
        return;
    }
    // Every enabled branch is already in the review worktree, so the final
    // summary (if one exists) is strictly more informed than anything this
    // git-log pass can produce -- leave it alone rather than downgrading it.
    let all_rebased = candidates.iter().filter(|c| c.reported).all(|c| c.rebased);
    if all_rebased && has_final_summary {
        return;
    }

    let ws_root = Workspace::for_guardian(store, id, PathBuf::from(&git_root));
    // Group by project, preserving stack order within each and the order the
    // projects first appear -- mirrors `generate_final_summary`'s grouping.
    let mut project_order: Vec<String> = Vec::new();
    for c in &candidates {
        if !project_order.contains(&c.project) {
            project_order.push(c.project.clone());
        }
    }

    // (branch, its own commit subjects) in stack order, deduped below.
    let mut sections: Vec<(String, Vec<String>)> = Vec::new();
    for proj in &project_order {
        let root = ws_root.at(PathBuf::from(proj));
        // Two independent running bases, one per ref source. `prev_review` is
        // seeded at the commit this project's review was cut from and walks
        // the review refs; `prev_task` is seeded at the base branch and walks
        // the producing worktrees' HEADs. RAL-147: each branch's worktree is
        // built on top of the previous branch in the stack, so diffing every
        // branch against a fixed base would make each section accumulate all
        // the earlier branches' commits.
        let mut prev_review = base_commits.get(proj).cloned();
        let mut prev_task = base_branch.clone();
        for c in candidates.iter().filter(|c| &c.project == proj) {
            let task_ws = c.cwd.as_ref().map(|cwd| {
                Workspace::on(Path::new(cwd), c.machine.as_deref()).with_store(Arc::clone(store))
            });
            // A disabled branch is not in the review, so it contributes no
            // section -- but the producing worktrees are stacked on disk
            // regardless of what is enabled, so the branch above it still
            // carries its commits. Step the task-side base over it (the review
            // side already skips it, since the rebase drops it from the stack)
            // or those commits surface under whichever branch comes next.
            if !c.reported {
                if let Some(Ok(head_sha)) =
                    task_ws.as_ref().map(|ws| ws.git(&["rev-parse", "HEAD"]))
                {
                    prev_task = head_sha.trim().to_string();
                }
                continue;
            }
            let review_ref = c.review_ref.as_str();
            // A `skip_worktrees` project never gets per-branch refs, so this
            // read fails and the branch falls back to its task worktree --
            // which still gives it its own section, rather than being lumped
            // in with its siblings the way the final summary has to do.
            let from_review = if c.rebased {
                prev_review.as_ref().and_then(|prev| {
                    root.git(&["log", "--format=%s", &format!("{prev}..{review_ref}")])
                        .ok()
                })
            } else {
                None
            };
            let log = match from_review {
                Some(log) => {
                    prev_review = Some(review_ref.to_string());
                    log
                }
                None => {
                    let Some(ws) = task_ws.as_ref() else {
                        continue;
                    };
                    ws.git(&["log", "--format=%s", &format!("{prev_task}..HEAD")])
                        .unwrap_or_default()
                }
            };
            // Advance the task-side base even for a branch reported from its
            // review ref, so a later un-rebased branch stacked on this one's
            // worktree still reports only its own commits.
            if let Some(Ok(head_sha)) = task_ws.as_ref().map(|ws| ws.git(&["rev-parse", "HEAD"])) {
                prev_task = head_sha.trim().to_string();
            }
            let subjects: Vec<String> = log
                .lines()
                .map(str::trim)
                .filter(|l| !l.is_empty())
                .map(str::to_string)
                .collect();
            if !subjects.is_empty() {
                sections.push((c.branch.clone(), subjects));
            }
        }
    }

    // Strip duplicate commits: the first branch in stack order to carry a
    // subject keeps it, later branches drop it. A branch left with nothing of
    // its own drops out entirely rather than showing an empty heading.
    let mut seen: HashSet<String> = HashSet::new();
    let sections: Vec<String> = sections
        .into_iter()
        .filter_map(|(branch, subjects)| {
            let unique: Vec<String> = subjects
                .into_iter()
                .filter(|s| seen.insert(s.clone()))
                .collect();
            (!unique.is_empty()).then(|| format!("{branch}:\n{}", unique.join("\n")))
        })
        .collect();
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

/// RAL-303: a settled review whose `change_summary` was never upgraded past
/// the git-log preliminary one (`summary_agent` unset) is stuck — the only
/// thing that requests an LLM summary is a change to the enabled-branch set,
/// so nothing will ever ask again on its own. That happens whenever the daemon
/// restarts between [`queue_final_summary_regen`] and the
/// [`sweep_pending_summaries`] tick that would have fired it, since the
/// debounce bookkeeping is in-memory.
///
/// Recompute the preliminary summary first so the review immediately shows one
/// section per branch rather than whatever partial snapshot it was left with,
/// then request the LLM summary that supersedes it.
/// [`Store::claim_final_summary_repair`] bounds this to one attempt per
/// guardian per daemon process.
fn repair_missing_final_summary(store: &Arc<Mutex<Store>>, id: &str) {
    let needs_repair = store
        .lock()
        .expect("poisoned")
        .get_guardian(id)
        .is_ok_and(|g| g.summary_agent.is_none());
    if !needs_repair {
        return;
    }
    if !store
        .lock()
        .expect("poisoned")
        .claim_final_summary_repair(id)
    {
        return;
    }
    crate::rlog!(
        INFO,
        "ralphus [guardian] review {id} has no agent-authored change summary: recomputing"
    );
    {
        let guard = store.lock().expect("poisoned");
        crate::cartographer::Note::new("guardian")
            .guardian(id)
            .scope("guardian")
            .emit(
                &guard,
                "review has no agent-authored change summary: recomputing",
                serde_json::json!({}),
            );
    }
    recompute_preliminary_summary(store, id);
    queue_final_summary_regen(store, id);
}

/// RAL-208: minimum quiet period after the last [`queue_final_summary_regen`]
/// request before [`sweep_pending_summaries`] actually fires the LLM call --
/// keeps rapid enable/disable toggling (each of which triggers a full
/// `run_merge`) from producing one LLM call per toggle. The debounce clock
/// restarts on every new request for the same guardian, so only the toggle
/// that settles for this long actually regenerates the summary.
const FINAL_SUMMARY_DEBOUNCE_MS: i64 = 5_000;

/// RAL-208: identifies which branches (and in what stack order) fed a
/// guardian's LLM-authored final change summary. Two builds with the same
/// enabled branches in the same order hash to the same signature, so
/// [`Store::request_final_summary`] recognises a rebuild that didn't actually
/// add/remove/reorder a branch (a feedback restack, a manual-push rebase, a
/// base-branch shift) and skips requeuing a regen for it -- only an actual
/// enable/disable changes the signature.
fn enabled_branch_signature(branches: &[crate::guardian::BranchView]) -> String {
    branches
        .iter()
        .filter(|b| b.enabled)
        .map(|b| b.id.as_str())
        .collect::<Vec<_>>()
        .join(",")
}

/// RAL-208: request that guardian `id`'s LLM-authored final change summary be
/// regenerated to reflect its current enabled-branch set. Safe to call after
/// every rebuild (a plain re-stack, a manual-push rebase, a base-branch
/// shift, an enable/disable toggle) -- [`Store::request_final_summary`] is a
/// no-op unless the enabled-branch set actually changed since the summary
/// currently stored was generated, and otherwise (re)starts the debounce
/// window that [`sweep_pending_summaries`] waits out before actually calling
/// the LLM.
pub(crate) fn queue_final_summary_regen(store: &Arc<Mutex<Store>>, id: &str) {
    let Ok(guardian) = store.lock().expect("poisoned").get_guardian(id) else {
        return;
    };
    let signature = enabled_branch_signature(&guardian.branches);
    // RAL-303: a review still showing the deterministic git-log preliminary
    // summary has never had the LLM pass run over it, so the signature check
    // has nothing meaningful to compare against and would wrongly decide the
    // summary is already up to date. That is the whole handoff this call
    // exists to perform once the stack has finished rebuilding -- force it.
    let force = guardian.summary_agent.is_none();
    store.lock().expect("poisoned").request_final_summary(
        id,
        &signature,
        crate::store::now_ms(),
        force,
    );
}

/// RAL-208: fire the LLM change-summary call for every guardian whose
/// debounce window (see [`FINAL_SUMMARY_DEBOUNCE_MS`]) has elapsed since its
/// last [`queue_final_summary_regen`] request. Called periodically from
/// `scheduler::run_loop`, mirroring [`review_maintenance`]'s pattern: this
/// function itself only claims the due ids (a quick, non-blocking store op)
/// and spawns one thread per guardian -- gated on `sem`, the same global
/// concurrency cap every other LLM call shares -- to actually run the
/// (potentially slow) LLM call, so the scheduler tick that called this never
/// blocks on one.
pub fn sweep_pending_summaries(store: &Arc<Mutex<Store>>, sem: &Arc<Semaphore>) {
    let due = store
        .lock()
        .expect("poisoned")
        .take_due_final_summary_requests(crate::store::now_ms(), FINAL_SUMMARY_DEBOUNCE_MS);
    for (id, signature) in due {
        let store = Arc::clone(store);
        let sem = Arc::clone(sem);
        std::thread::spawn(move || {
            let _permit = sem.acquire();
            let runner: Arc<dyn Runner> = Arc::new(
                crate::runner::SubprocessRunner::from_env().with_cartographer(Arc::clone(&store)),
            );
            generate_final_summary(&store, runner.as_ref(), &id, &signature);
        });
    }
}

/// RAL-208: (re)generate guardian `id`'s LLM-authored final change summary
/// across every project it spans, from the persisted `guardian/<id>/wt-<branch>`
/// review refs (falling back to the shared `guardian/<id>/review` combined ref
/// for a project built with `skip_worktrees`, which never gets per-branch
/// refs). Reconstructed fresh from the store rather than carried from a
/// `run_merge` call's local state, since this runs on a background sweep well
/// after that call's thread has already finished.
///
/// A single call covers every enabled branch across every project -- unlike
/// the pre-RAL-208 per-project calls, which each overwrote the previous
/// project's result in a multi-project guardian, this can never leave any
/// project's branches unrepresented in the final summary.
///
/// The result is stored as `change_summary` on the guardian and surfaced in
/// the review detail pane. Failures are silent — a missing summary is better
/// than a crashed sweep -- and leave `signature` unmarked, so the next
/// request for the same signature (e.g. the next sweep tick, or a future
/// unrelated toggle) retries rather than being treated as already-satisfied.
///
/// RAL-53: uses commit subject lines only (no diffs) so the output describes
/// developer intent rather than low-level file changes.
///
/// RAL-124: whether the output is a one-bullet-per-branch list (default) or
/// the original prose paragraph is controlled by `[review] summary_format` in
/// `.ralphus.toml`/global config (see [`crate::config::ReviewConfig::bullet_summary`]).
/// In bullet mode, each branch is labelled with [`branch_summary_label`] --
/// its ticket id when the branch name starts with one, else the branch name
/// itself -- and the agent is instructed to use that exact label per bullet.
fn generate_final_summary(
    store: &Arc<Mutex<Store>>,
    runner: &dyn Runner,
    id: &str,
    signature: &str,
) {
    let Ok(guardian) = store.lock().expect("poisoned").get_guardian(id) else {
        return;
    };
    if guardian.status != GuardianStatus::InReview.as_str() {
        return;
    }
    let enabled: Vec<&crate::guardian::BranchView> =
        guardian.branches.iter().filter(|b| b.enabled).collect();
    if enabled.is_empty() {
        return;
    }
    let ws_root = Workspace::for_guardian(store, id, PathBuf::from(&guardian.git_root));

    // Group by project, preserving stack (position) order within each project
    // and the order projects first appear -- mirrors `run_merge`'s own grouping.
    let mut project_order: Vec<String> = Vec::new();
    let mut by_project: std::collections::HashMap<String, Vec<&crate::guardian::BranchView>> =
        std::collections::HashMap::new();
    for b in &enabled {
        let proj = b
            .project
            .clone()
            .unwrap_or_else(|| guardian.git_root.clone());
        if !by_project.contains_key(&proj) {
            project_order.push(proj.clone());
        }
        by_project.entry(proj).or_default().push(b);
    }

    let mut lines: Vec<String> = Vec::new();
    for proj in &project_order {
        let branches = &by_project[proj];
        let Some(base_sha) = guardian.base_commits.get(proj) else {
            continue;
        };
        let root = ws_root.at(PathBuf::from(proj));
        let first_ref = review_ref_of(id, branches[0]);
        if root.git(&["rev-parse", "--verify", &first_ref]).is_ok() {
            let mut prev = base_sha.clone();
            for b in branches {
                let branch_ref = review_ref_of(id, b);
                // RAL-201: was `git(root.root(), ...)`, a direct bypass of
                // `root`'s machine.
                let b_log = root
                    .git(&["log", "--format=%s", &format!("{prev}..{branch_ref}")])
                    .unwrap_or_default();
                if !b_log.trim().is_empty() {
                    lines.push(format!(
                        "{}:\n{}",
                        branch_summary_label(&b.branch),
                        b_log.trim()
                    ));
                }
                prev = branch_ref;
            }
        } else {
            // `skip_worktrees`: no per-branch refs exist for this project --
            // fall back to the shared combined ref, labelling the section
            // with every branch this project contributed.
            let combined_ref = combined_review_ref_of(&guardian);
            // RAL-201: was `git(root.root(), ...)`, a direct bypass of
            // `root`'s machine.
            let log = root
                .git(&["log", "--format=%s", &format!("{base_sha}..{combined_ref}")])
                .unwrap_or_default();
            if !log.trim().is_empty() {
                let labels = branches
                    .iter()
                    .map(|b| branch_summary_label(&b.branch))
                    .collect::<Vec<_>>()
                    .join(", ");
                lines.push(format!("{labels}:\n{}", log.trim()));
            }
        }
    }
    if lines.is_empty() {
        return;
    }
    let context = lines.join("\n\n");
    let branch_labels = enabled
        .iter()
        .map(|b| branch_summary_label(&b.branch))
        .collect::<Vec<_>>()
        .join(", ");

    let cwd = ws_root.root().to_string_lossy().into_owned();
    let resolved = match resolve_resolver_agent(
        guardian.resolver_agent.as_deref(),
        guardian.resolver_model.as_deref(),
        ws_root.root(),
    ) {
        Ok(r) => r,
        Err(message) => {
            crate::rlog!(
                WARNING,
                "ralphus [guardian] review {id} summary generation: unresolvable resolver agent: {message}"
            );
            return;
        }
    };
    let (agent, model) = (resolved.backend.clone(), resolved.model.clone());

    let prompt = if crate::config::resolve(ws_root.root()).bullet_summary() {
        format!(
            "You are summarising a stacked code review made up of the branches \
             [{branch_labels}]. The following are commit subject lines for each \
             branch — one line per commit. Write the summary as a bullet list \
             with EXACTLY one bullet per branch, each on its own line in the \
             form `- <label>: <description>`, where <label> is exactly one of \
             the branch labels given above (do not invent or reformat it). \
             Each bullet must be a SINGLE LINE, no more than 80 characters \
             total, focused on developer intent, not file-level details. You \
             may simplify or collapse redundant detail within a bullet, but \
             every branch listed above must be represented by exactly one \
             bullet. Respond with ONLY the bullet list — no preamble, no \
             trailing remarks.\
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
        // RAL-102: unique per guardian — a bare "guardian" squad_id collides
        // with every other guardian's tmux session name (observed in CI as
        // cross-test contamination when two live-Ollama tests generate a
        // summary concurrently and clobber each other's tmux session).
        squad_id: format!("guardian-{id}"),
        task: "summary".to_string(),
        cell_id: "summarizer".to_string(),
        cwd,
        prompt: Some(prompt),
        command: None,
        agent: agent.clone(),
        executable: resolved.executable.clone(),
        model: model.clone(),
        system_prompt: None,
        system_prompt_position: None,
        timeout_sec: None,
        budget_tokens: None,
        maximum_budget_usd: None,
        maximum_context: None,
        auto_compact_threshold: None,
        maximum_tool_output_tokens: None,
        proof: false,
        trace_context: None,
        resume_agent_session_id: None,
        assigned_agent_session_id: None,
        env_overrides: resolved.env.clone(),
        // RAL-201: route to the same machine `cwd` (derived from `ws_root`)
        // is actually on -- see the identical fix in
        // `resolve_conflicts_with_agent`.
        machine: ws_root.machine().map(str::to_string),
        tool_arg_truncate_chars: None,
        thrash_max_compactions: None,
        thrash_min_turn_gap: None,
        allow_personal_settings: false,
        allow_personal_memory: false,
    };
    let result = runner.run(&spec);
    let _ = record_guardian_call_cost(store, id, None, "summary", &result);
    if result.is_done() && !result.summary.trim().is_empty() {
        // RAL-88: record which resolved agent/model produced this summary.
        let mut guard = store.lock().expect("poisoned");
        let _ =
            guard.set_guardian_summary(id, &result.summary, Some(agent.as_str()), model.as_deref());
        guard.mark_final_summary_generated(id, signature);
    } else {
        // RAL-303: the review is left showing its git-log preliminary summary,
        // which reads as a legitimately-generated one -- say why in the log
        // rather than failing silently.
        crate::rlog!(
            WARNING,
            "ralphus [guardian] review {id} summary generation produced nothing ({agent}, status {}): keeping the preliminary summary",
            result.status
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
            // The resolver-agent JSON contract doesn't ask for a type today
            // (RAL-221) -- every LLM-synthesized input stays unconstrained
            // `String` until that contract is extended.
            r#type: CheckInputType::String,
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

/// JSON shape asked of the resolver agent for manual-command suggestions
/// (RAL-164) — see [`generate_manual_commands`].
#[derive(Deserialize)]
struct ManualChecksInference {
    manual_commands: Vec<ManualCheckItem>,
}

/// Parse the resolver agent's manual-commands response. Tries the
/// `{"manual_commands": [...]}` object shape first (with a `{...}`-substring
/// fallback for chatty models), then falls back to the original bare
/// `["...", ...]` array shape for backward compatibility and for small local
/// models that ignore the object-shape instruction. Each `manual_commands`
/// element is either a bare string or a RAL-164 structured object (see
/// [`ManualCheckItem`]). Returns the parsed checks, empty when nothing
/// parseable was found.
fn parse_manual_commands_response(text: &str) -> Vec<GuardianCheck> {
    let as_object = serde_json::from_str::<ManualChecksInference>(text)
        .ok()
        .or_else(|| {
            let start = text.find('{')?;
            let end = text.rfind('}').unwrap_or(text.len().saturating_sub(1));
            serde_json::from_str::<ManualChecksInference>(&text[start..=end]).ok()
        });
    if let Some(obj) = as_object {
        return obj.manual_commands.into_iter().map(Into::into).collect();
    }
    let as_array = serde_json::from_str::<Vec<ManualCheckItem>>(text)
        .ok()
        .or_else(|| {
            let start = text.find('[')?;
            let end = text.rfind(']').unwrap_or(text.len().saturating_sub(1));
            serde_json::from_str::<Vec<ManualCheckItem>>(&text[start..=end]).ok()
        });
    as_array
        .unwrap_or_default()
        .into_iter()
        .map(Into::into)
        .collect()
}

/// Shared prompt body for [`generate_manual_commands`].
fn manual_commands_prompt(tail: &str) -> String {
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
    let format = " Return ONLY a valid JSON array where each element is either a plain string or \
          the object shape described above — no markdown fences, no explanation, no \
          other text.";
    format!("{focus}{format}\n\n{tail}")
}

/// Generate LLM-suggested shell commands for manually testing or verifying
/// the changes in the review branch (RAL-27).
///
/// `worktree` is the combined review worktree path (preferred). When present the
/// LLM runs inside the worktree with its `run_bash` tool so it can inspect the
/// diff itself — no diff content is embedded in the prompt, which avoids OS
/// command-line length limits in harness backends. Falls back to a file-name
/// list from `root` when no worktree is available.
///
/// Generation failures are silent — a missing command list is better than a
/// crash.
#[allow(clippy::too_many_arguments)]
fn generate_manual_commands(
    store: &Arc<Mutex<Store>>,
    runner: &dyn Runner,
    id: &str,
    root: &Workspace,
    base_sha: &str,
    tip_ref: &str,
    worktree: Option<&Workspace>,
    cancel: &CancelToken,
) {
    let (cwd, machine, prompt) = if let Some(wt) = worktree {
        // Worktree path: embed only the --stat output (always compact — one line
        // per changed file). Never embed the full diff; it can be arbitrarily
        // large and would blow OS command-line limits in harness backends.
        let stat = wt.git(&["diff", "--stat", base_sha]).unwrap_or_default();
        if stat.trim().is_empty() {
            return;
        }
        // RAL-201: was `git(root.root(), ...)`, a direct bypass of `root`'s
        // machine -- `root.git(...)` routes through the provider when `root`
        // is remote, exactly like the `wt.git(...)` call just above.
        let log = root
            .git(&["log", "--format=%s", &format!("{base_sha}..{tip_ref}")])
            .unwrap_or_default();
        let tail = format!("Changed files (stat):\n{stat}\n\nCommit messages:\n{log}");
        (
            wt.root().to_string_lossy().into_owned(),
            wt.machine().map(str::to_string),
            manual_commands_prompt(&tail),
        )
    } else {
        // Fallback: list changed file names from the repository root. The file
        // list is always small, so it is safe to embed directly.
        // RAL-201: same `root.git(...)` fix as above.
        let files = match root.git(&["diff", "--name-only", &format!("{base_sha}..{tip_ref}")]) {
            Ok(s) if !s.trim().is_empty() => s,
            _ => return,
        };
        let log = root
            .git(&["log", "--format=%s", &format!("{base_sha}..{tip_ref}")])
            .unwrap_or_default();
        let tail = format!("Changed files:\n{files}\n\nCommit messages:\n{log}");
        (
            root.root().to_string_lossy().into_owned(),
            root.machine().map(str::to_string),
            manual_commands_prompt(&tail),
        )
    };

    let resolved = {
        let stored_agent;
        let stored_model;
        {
            let guard = store.lock().expect("poisoned");
            let g = guard.get_guardian(id).ok();
            stored_agent = g.as_ref().and_then(|g| g.resolver_agent.clone());
            stored_model = g.and_then(|g| g.resolver_model.clone());
        }
        match resolve_resolver_agent(
            stored_agent.as_deref(),
            stored_model.as_deref(),
            Path::new(&cwd),
        ) {
            Ok(r) => r,
            Err(message) => {
                crate::cartographer::Note::new("guardian")
                    .level(crate::logging::LogLevel::WARNING)
                    .guardian(id)
                    .scope("guardian")
                    .emit(
                        &store.lock().expect("poisoned"),
                        format!(
                            "review {id} manual-commands generation: unresolvable \
                             resolver agent: {message}"
                        ),
                        serde_json::json!({"error": message}),
                    );
                return;
            }
        }
    };
    let (agent, model) = (resolved.backend.clone(), resolved.model.clone());

    let spec = RunnerSpec {
        // RAL-102/RAL-88 follow-up: unique per guardian (see the comment on
        // the resolver `RunnerSpec` in `resolve_conflicts_with_agent`) so this
        // generation's tmux session never collides with another guardian's.
        squad_id: format!("guardian-{id}"),
        task: MANUAL_COMMANDS_TASK.to_string(),
        cell_id: MANUAL_COMMANDS_SESSION.to_string(),
        cwd,
        prompt: Some(prompt),
        command: None,
        agent: agent.clone(),
        executable: resolved.executable.clone(),
        model: model.clone(),
        system_prompt: None,
        system_prompt_position: None,
        timeout_sec: None,
        budget_tokens: None,
        maximum_budget_usd: None,
        maximum_context: None,
        auto_compact_threshold: None,
        maximum_tool_output_tokens: None,
        proof: false,
        trace_context: None,
        resume_agent_session_id: None,
        assigned_agent_session_id: None,
        env_overrides: resolved.env.clone(),
        // RAL-201: matches whichever workspace `cwd` above was derived from.
        machine,
        tool_arg_truncate_chars: None,
        thrash_max_compactions: None,
        thrash_min_turn_gap: None,
        allow_personal_settings: false,
        allow_personal_memory: false,
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

    // RAL-259: the manual-checks generation agent is beginning to run — stamp
    // the guardian-level Live-View start time (plain overwrite, so a
    // regeneration always shows the latest generation's start).
    let _ = store
        .lock()
        .expect("poisoned")
        .stamp_guardian_manual_checks_started_at(id);
    let result = runner.run_cancellable(&spec, cancel);
    let _ = record_guardian_call_cost(store, id, None, "manual_commands", &result);

    stop.store(true, Ordering::Relaxed);
    let _ = watcher.join();

    if let Some(sid) = result.agent_session_id.as_deref() {
        let _ = store
            .lock()
            .expect("poisoned")
            .set_guardian_manual_commands_session_id(id, sid);
    }

    if !result.is_done() || result.summary.trim().is_empty() {
        return;
    }

    let commands = parse_manual_commands_response(result.summary.trim());

    if !commands.is_empty() {
        // RAL-88: record which resolved agent/model produced these commands.
        let _ = store
            .lock()
            .expect("poisoned")
            .set_guardian_manual_commands(id, &commands, Some(agent.as_str()), model.as_deref());
    }
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
    let Some((cwd, stored_agent, stored_model, machine)) = ({
        let guard = store.lock().expect("poisoned");
        guard.get_guardian(guardian_id).ok().map(|g| {
            let cwd = g.combined_worktree.clone().unwrap_or(g.git_root.clone());
            (cwd, g.resolver_agent, g.resolver_model, g.machine)
        })
    }) else {
        let guard = store.lock().expect("poisoned");
        let _ = guard.set_guardian_input_resolution_failed(guardian_id, &input.name);
        return;
    };
    let resolved = match resolve_resolver_agent(
        stored_agent.as_deref(),
        stored_model.as_deref(),
        Path::new(&cwd),
    ) {
        Ok(r) => r,
        Err(message) => {
            // ralphus[ignore-rlog-pair]: this low-level helper has no Store; its Store-owning caller records the structured workflow outcome
            crate::rlog!(
                WARNING,
                "ralphus [guardian] review {guardian_id} check-input resolution: \
                 unresolvable resolver agent: {message}"
            );
            let guard = store.lock().expect("poisoned");
            let _ = guard.set_guardian_input_resolution_failed(guardian_id, &input.name);
            return;
        }
    };

    let spec = RunnerSpec {
        // Unique per (guardian, input) so concurrent resolutions for
        // different inputs on the same guardian -- or the guardian's own
        // manual-commands generation -- never collide on one tmux session.
        squad_id: format!("guardian-{guardian_id}-input-{}", input.name),
        task: RESOLVE_INPUT_TASK.to_string(),
        cell_id: format!("resolve-input-{}", input.name),
        cwd,
        prompt: Some(resolve_input_prompt(command, input)),
        command: None,
        agent: resolved.backend.clone(),
        executable: resolved.executable.clone(),
        model: resolved.model.clone(),
        system_prompt: None,
        system_prompt_position: None,
        timeout_sec: None,
        budget_tokens: None,
        maximum_budget_usd: None,
        maximum_context: None,
        auto_compact_threshold: None,
        maximum_tool_output_tokens: None,
        proof: false,
        trace_context: None,
        resume_agent_session_id: None,
        assigned_agent_session_id: None,
        env_overrides: resolved.env.clone(),
        // RAL-201: route to the review's assigned machine, matching `cwd`.
        machine,
        tool_arg_truncate_chars: None,
        thrash_max_compactions: None,
        thrash_min_turn_gap: None,
        allow_personal_settings: false,
        allow_personal_memory: false,
    };

    let result = runner.run(&spec);
    let _ = record_guardian_call_cost(store, guardian_id, None, "check_input", &result);
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

    fn write_file(root: &Path, rel: &str, content: &str) {
        std::fs::write(root.join(rel), content).expect("write test file");
    }

    fn rev_parse(root: &Path, rev: &str) -> String {
        String::from_utf8(
            std::process::Command::new("git")
                .args(["rev-parse", rev])
                .current_dir(root)
                .output()
                .expect("git rev-parse")
                .stdout,
        )
        .expect("utf8")
        .trim()
        .to_string()
    }

    // -----------------------------------------------------------------------
    // RAL-342/RAL-338: resolver_model's project-config default tier
    // -----------------------------------------------------------------------

    #[test]
    fn resolver_model_falls_back_to_the_project_default_when_stored_and_env_are_unset() {
        let dir = tmp_dir("resolver-model");
        std::fs::write(
            dir.join(".ralphus.toml"),
            "[review]\ndefault_resolver_model = \"claude-haiku-4-5\"\n",
        )
        .unwrap();

        assert_eq!(
            resolver_model(None, "claude-code", &dir),
            Some("claude-haiku-4-5".to_string())
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn resolver_model_stored_value_wins_over_the_project_default() {
        let dir = tmp_dir("resolver-model-stored");
        std::fs::write(
            dir.join(".ralphus.toml"),
            "[review]\ndefault_resolver_model = \"claude-haiku-4-5\"\n",
        )
        .unwrap();

        assert_eq!(
            resolver_model(Some("qwen3:8b"), "claude-code", &dir),
            Some("qwen3:8b".to_string())
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn resolver_model_with_no_project_default_still_falls_back_to_the_ollama_default() {
        let dir = tmp_dir("resolver-model-none");
        assert_eq!(
            resolver_model(None, "ollama", &dir),
            Some("qwen3:8b".to_string())
        );
        assert_eq!(resolver_model(None, "claude-code", &dir), None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    // -----------------------------------------------------------------------
    // RAL-330: rerere fast-path content-preservation guard
    //
    // `REBASE_HEAD` is faked directly (`.git/REBASE_HEAD`) rather than driving
    // a real conflicting rebase to that exact paused state — `git rev-parse
    // REBASE_HEAD` just reads that file, so this exercises the same plumbing
    // the production code relies on while keeping each scenario a
    // deterministic, hand-built commit graph instead of depending on git's
    // own conflict-resolution behavior to land in a specific spot.
    // -----------------------------------------------------------------------

    /// Builds: `old_parent` (root) -> `rebase_head` (modifies shared.txt,
    /// adds new_file.txt) on one side, and `old_parent` -> `new_base` (adds
    /// other.txt, optionally also touches shared.txt) on the other. `HEAD` is
    /// left checked out on `new_base`, and `.git/REBASE_HEAD` is set to
    /// `rebase_head` -- i.e. exactly the state the resolver loop sees while
    /// paused partway through replaying `rebase_head` onto `new_base`.
    /// Returns `(root, rebase_head_sha)`.
    fn setup_fake_rebase_pause(new_base_also_touches_shared: bool) -> (PathBuf, String) {
        let root = tmp_dir("ral330");
        g(&root, &["init", "--quiet", "--initial-branch", "main"]);
        g(&root, &["config", "user.email", "t@t.com"]);
        g(&root, &["config", "user.name", "t"]);

        write_file(&root, "shared.txt", "line1\nBASE\nline3\n");
        g(&root, &["add", "."]);
        g(&root, &["commit", "--quiet", "--message", "base"]);
        let old_parent = rev_parse(&root, "HEAD");

        // The commit the resolver loop is (pretend-)paused on replaying.
        write_file(&root, "shared.txt", "line1\nFEATURE\nline3\n");
        write_file(&root, "new_file.txt", "brand new content\n");
        g(&root, &["add", "."]);
        g(&root, &["commit", "--quiet", "--message", "rebase_head"]);
        let rebase_head = rev_parse(&root, "HEAD");

        // The base this step is landing on -- built from old_parent directly,
        // so it never saw rebase_head's own changes.
        g(&root, &["checkout", "--quiet", &old_parent]);
        if new_base_also_touches_shared {
            write_file(&root, "shared.txt", "line1\nMAIN\nline3\n");
            g(&root, &["add", "shared.txt"]);
        }
        write_file(&root, "other.txt", "unrelated base content\n");
        g(&root, &["add", "other.txt"]);
        g(&root, &["commit", "--quiet", "--message", "new_base"]);

        std::fs::write(
            root.join(".git").join("REBASE_HEAD"),
            format!("{rebase_head}\n"),
        )
        .expect("write REBASE_HEAD");

        (root, rebase_head)
    }

    #[test]
    fn detect_rebase_step_content_loss_none_when_everything_is_staged() {
        let (root, _rebase_head) = setup_fake_rebase_pause(false);
        let wt = Workspace::local(&root);

        // Stage exactly what a correct replay of rebase_head onto new_base
        // would produce: shared.txt's own change (new_base never touched it)
        // plus the brand-new file.
        write_file(&root, "shared.txt", "line1\nFEATURE\nline3\n");
        write_file(&root, "new_file.txt", "brand new content\n");
        g(&root, &["add", "shared.txt", "new_file.txt"]);

        assert_eq!(detect_rebase_step_content_loss(&wt), None);

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn detect_rebase_step_content_loss_flags_a_dropped_unrelated_file() {
        let (root, _rebase_head) = setup_fake_rebase_pause(false);
        let wt = Workspace::local(&root);

        // Stage shared.txt's change correctly, but never stage new_file.txt
        // -- simulating whatever mechanism drops it (rerere fast-path replay,
        // or a short-circuit that fails to re-stage every resolved file).
        write_file(&root, "shared.txt", "line1\nFEATURE\nline3\n");
        g(&root, &["add", "shared.txt"]);

        assert_eq!(
            detect_rebase_step_content_loss(&wt),
            Some(vec!["new_file.txt".to_string()]),
            "a brand-new file the new base never touched must not silently vanish"
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn detect_rebase_step_content_loss_ignores_the_genuine_conflict_zone() {
        // new_base ALSO touches shared.txt this time, so it's a real conflict
        // zone: a correct merge is expected to differ from rebase_head's own
        // version, and must not be flagged just because the text changed.
        let (root, _rebase_head) = setup_fake_rebase_pause(true);
        let wt = Workspace::local(&root);

        // A plausible correct three-way resolution: neither side verbatim.
        write_file(&root, "shared.txt", "line1\nFEATURE and MAIN\nline3\n");
        write_file(&root, "new_file.txt", "brand new content\n");
        g(&root, &["add", "shared.txt", "new_file.txt"]);

        assert_eq!(
            detect_rebase_step_content_loss(&wt),
            None,
            "a real conflict's resolution differing from either side alone is not loss"
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn detect_rebase_step_content_loss_reports_real_paths_not_rename_display_syntax() {
        // RAL-330: `lost`'s entries are fed straight into `git rerere forget`
        // as literal pathspecs (see the call site above `detect_rebase_step_
        // content_loss`). If `rebase_head`'s own commit renamed a file, git's
        // default rename detection would otherwise report it as a single
        // `dir/{old => new}` display string -- not a usable pathspec, so
        // `rerere forget` silently matches nothing and the poisoned cache
        // entry survives to replay again on the very next retry.
        let root = tmp_dir("ral330-rename");
        g(&root, &["init", "--quiet", "--initial-branch", "main"]);
        g(&root, &["config", "user.email", "t@t.com"]);
        g(&root, &["config", "user.name", "t"]);

        std::fs::create_dir_all(root.join("dir")).expect("mkdir dir");
        write_file(&root, "dir/old_name.rs", "line1\nline2\n");
        g(&root, &["add", "."]);
        g(&root, &["commit", "--quiet", "--message", "base"]);
        let old_parent = rev_parse(&root, "HEAD");

        // The commit the resolver loop is (pretend-)paused on replaying:
        // renames the file and edits it, so git's default rename detection
        // pairs the two paths up in this commit's own diff.
        g(&root, &["mv", "dir/old_name.rs", "dir/new_name.rs"]);
        write_file(&root, "dir/new_name.rs", "line1\nline2\nline3\n");
        g(&root, &["add", "."]);
        g(&root, &["commit", "--quiet", "--message", "rebase_head"]);
        let rebase_head = rev_parse(&root, "HEAD");

        // new_base: built from old_parent directly, so it never saw
        // rebase_head's rename at all -- and HEAD stays checked out here,
        // simulating a fast-path replay that dropped the rename entirely
        // (the file is left exactly as new_base already has it).
        g(&root, &["checkout", "--quiet", &old_parent]);
        write_file(&root, "other.txt", "unrelated base content\n");
        g(&root, &["add", "other.txt"]);
        g(&root, &["commit", "--quiet", "--message", "new_base"]);

        std::fs::write(
            root.join(".git").join("REBASE_HEAD"),
            format!("{rebase_head}\n"),
        )
        .expect("write REBASE_HEAD");

        let wt = Workspace::local(&root);
        let lost = detect_rebase_step_content_loss(&wt).expect("must flag the dropped rename");

        assert!(
            lost.iter().all(|p| !p.contains("=>") && !p.contains('{')),
            "lost paths must be real, usable pathspecs -- got {lost:?}"
        );
        assert!(
            lost.contains(&"dir/new_name.rs".to_string()),
            "the renamed file's new content must be flagged as dropped -- got {lost:?}"
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn rebase_head_commit_is_empty_false_for_a_real_change() {
        let (root, _rebase_head) = setup_fake_rebase_pause(false);
        let wt = Workspace::local(&root);
        assert!(!rebase_head_commit_is_empty(&wt));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn rebase_head_commit_is_empty_true_for_an_actually_empty_commit() {
        let root = tmp_dir("ral330-empty");
        g(&root, &["init", "--quiet", "--initial-branch", "main"]);
        g(&root, &["config", "user.email", "t@t.com"]);
        g(&root, &["config", "user.name", "t"]);
        write_file(&root, "shared.txt", "content\n");
        g(&root, &["add", "."]);
        g(&root, &["commit", "--quiet", "--message", "base"]);
        g(
            &root,
            &["commit", "--quiet", "--allow-empty", "--message", "empty"],
        );
        let empty_sha = rev_parse(&root, "HEAD");
        std::fs::write(
            root.join(".git").join("REBASE_HEAD"),
            format!("{empty_sha}\n"),
        )
        .expect("write REBASE_HEAD");

        let wt = Workspace::local(&root);
        assert!(rebase_head_commit_is_empty(&wt));
        let _ = std::fs::remove_dir_all(&root);
    }

    // -----------------------------------------------------------------------
    // Guardian cost tracking (RAL-193)
    // -----------------------------------------------------------------------

    fn fake_result(tokens_in: i64, tokens_out: i64, cost_usd: f64) -> RunnerResult {
        RunnerResult {
            status: "done".into(),
            tokens_in,
            tokens_out,
            cache_creation_tokens: 0,
            cache_read_tokens: 0,
            compaction_input_tokens: 0,
            compaction_count: 0,
            cost_usd,
            cost_is_estimated: false,
            summary: String::new(),
            error: None,
            proofed: None,
            agent_session_id: None,
            ghost: None,
        }
    }

    #[test]
    fn record_guardian_call_cost_records_line_item_and_updates_view_totals() {
        let store = Arc::new(Mutex::new(Store::open_in_memory().unwrap()));
        let id = store
            .lock()
            .unwrap()
            .create_guardian("r", "main", "/repo")
            .unwrap();
        let result = fake_result(100, 40, 0.01);

        let outcome = record_guardian_call_cost(&store, &id, None, "resolve_conflict", &result);
        assert!(outcome.is_ok(), "{outcome:?}");

        let g = store.lock().unwrap().get_guardian(&id).unwrap();
        assert_eq!(g.cumulative_tokens_in, 100);
        assert_eq!(g.cumulative_tokens_out, 40);
        assert!((g.cumulative_cost_usd - 0.01).abs() < 1e-9);
        // No merge attempt was ever bumped -- still attributed to attempt 0.
        assert_eq!(g.attempt_tokens_in, 100);
    }

    #[test]
    fn record_guardian_call_cost_without_a_cap_never_errors() {
        let store = Arc::new(Mutex::new(Store::open_in_memory().unwrap()));
        let id = store
            .lock()
            .unwrap()
            .create_guardian("r", "main", "/repo")
            .unwrap();
        let expensive = fake_result(1_000_000, 1_000_000, 500.0);
        assert!(
            record_guardian_call_cost(&store, &id, None, "resolve_conflict", &expensive).is_ok()
        );
    }

    #[test]
    fn record_guardian_call_cost_errors_once_cumulative_exceeds_cap() {
        let store = Arc::new(Mutex::new(Store::open_in_memory().unwrap()));
        let id = store
            .lock()
            .unwrap()
            .create_guardian("r", "main", "/repo")
            .unwrap();
        store
            .lock()
            .unwrap()
            .set_guardian_maximum_budget_usd(&id, Some(0.05))
            .unwrap();

        // Under the cap: no error, and it's still recorded.
        let under = fake_result(10, 10, 0.02);
        assert!(record_guardian_call_cost(&store, &id, None, "resolve_conflict", &under).is_ok());

        // This call's own cost pushes the cumulative total over the cap.
        let pushes_over = fake_result(10, 10, 0.05);
        let err = record_guardian_call_cost(&store, &id, None, "proof", &pushes_over);
        assert!(err.is_err());
        assert!(err.unwrap_err().contains("maximum_budget_usd"));

        // Both calls were still recorded despite the second exceeding the cap --
        // the caller decides whether/how to stop, this function only reports it.
        let (_, _, cumulative) = store.lock().unwrap().guardian_cost_total(&id).unwrap();
        assert!((cumulative - 0.07).abs() < 1e-9);
    }

    #[test]
    fn record_guardian_call_cost_attributes_to_current_merge_attempt() {
        let store = Arc::new(Mutex::new(Store::open_in_memory().unwrap()));
        let id = store
            .lock()
            .unwrap()
            .create_guardian("r", "main", "/repo")
            .unwrap();
        store
            .lock()
            .unwrap()
            .bump_guardian_merge_attempt(&id)
            .unwrap(); // attempt 1
        record_guardian_call_cost(
            &store,
            &id,
            None,
            "resolve_conflict",
            &fake_result(10, 5, 0.01),
        )
        .unwrap();
        store
            .lock()
            .unwrap()
            .bump_guardian_merge_attempt(&id)
            .unwrap(); // attempt 2
        record_guardian_call_cost(
            &store,
            &id,
            None,
            "resolve_conflict",
            &fake_result(20, 8, 0.02),
        )
        .unwrap();

        let g = store.lock().unwrap().get_guardian(&id).unwrap();
        assert_eq!(g.merge_attempt, 2);
        assert_eq!(g.attempt_tokens_in, 20); // attempt 2 only
        assert_eq!(g.attempt_tokens_out, 8);
        assert_eq!(g.cumulative_tokens_in, 30); // both attempts
        assert_eq!(g.cumulative_tokens_out, 13);
    }

    // -----------------------------------------------------------------------
    // Manual-checks response parsing (RAL-164)
    // -----------------------------------------------------------------------

    #[test]
    fn parse_manual_commands_bare_string_array_back_compat() {
        let checks = parse_manual_commands_response(r#"["cargo test", "npm run e2e"]"#);
        assert_eq!(checks.len(), 2);
        assert_eq!(checks[0].command.as_deref(), Some("cargo test"));
        assert!(checks[0].inputs.is_empty());
        assert!(checks[0].cleanup_command.is_none());
        assert_eq!(checks[1].command.as_deref(), Some("npm run e2e"));
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
            ]
        }"#;
        let checks = parse_manual_commands_response(text);
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
    }

    #[test]
    fn parse_manual_commands_tolerates_chatty_model_wrapping_json_in_prose() {
        let text = "Sure, here you go:\n```json\n{\"manual_commands\": [\"cargo test\"]}\n```\nHope that helps!";
        let checks = parse_manual_commands_response(text);
        assert_eq!(checks.len(), 1);
        assert_eq!(checks[0].command.as_deref(), Some("cargo test"));
    }

    #[test]
    fn parse_manual_commands_unparseable_text_is_empty() {
        let checks = parse_manual_commands_response("not json at all");
        assert!(checks.is_empty());
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
                cache_creation_tokens: 0,
                cache_read_tokens: 0,
                compaction_input_tokens: 0,
                compaction_count: 0,
                cost_usd: 0.0,
                cost_is_estimated: false,
                summary: self.0.to_string(),
                error: None,
                proofed: None,
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
                        r#type: CheckInputType::Int,
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
    // Deliberately real `git` throughout, not libgit2: this module's own
    // `a_full_merge_completes_from_a_repository_root_long_enough_to_have_failed_pre_ral_211`
    // exercises the RAL-211 Windows long-path fix by giving `make_repo` a
    // tag long enough to approach `MAX_PATH`. libgit2 hits its own internal
    // path-length limit well before real `git.exe` does on Windows (no
    // extended-length-prefix support for these calls), so switching this
    // fixture to git2 would silently defeat the regression test it exists
    // to protect.
    fn make_repo(tag: &str) -> (PathBuf, PathBuf, PathBuf) {
        let base = tmp_dir(tag);
        let repo = base.join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        g(&repo, &["init", "--initial-branch", "main"]);
        std::fs::write(repo.join("base.txt"), "base\n").unwrap();
        g(&repo, &["add", "."]);
        g(&repo, &["commit", "--message", "base"]);
        let fwt = base.join("fwt");
        g(
            &repo,
            &["worktree", "add", "-b", "feature/a", fwt.to_str().unwrap()],
        );
        std::fs::write(fwt.join("feat.txt"), "feat\n").unwrap();
        g(&fwt, &["add", "."]);
        g(&fwt, &["commit", "--message", "feature"]);
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

    /// State 1 combined with state 7: the worktree directory is gone (e.g. an
    /// external cleanup deleted it) but its tracking entry was left locked, so
    /// the still-registered branch blocks a plain `worktree add -B` at that
    /// path with `fatal: '<branch>' is already used by worktree at '<path>'`
    /// even though nothing is actually checked out there anymore. The
    /// directory-missing branch of `worktree_add_or_reset_with_faults` did not
    /// call `worktree unlock` before its `remove`/`prune` cleanup (unlike the
    /// directory-survives branch just below it, which does), so `remove -f`
    /// (single force) left the locked entry in place and `prune` silently
    /// skips locked entries by design — the retry-after-prune inside the
    /// state-1 branch could never clear it. Reproduces the exact failure seen
    /// live during a guardian's combined-review-worktree rebuild.
    #[test]
    fn worktree_recovery_state1_directory_missing_while_locked() {
        let (base, repo, _fwt) = make_repo("s1locked");
        let rwt = base.join("rwt");

        // Initial setup.
        worktree_add_or_reset(
            &Workspace::local(&repo),
            "guardian/g/wt-feature-a",
            &Workspace::local(&rwt),
            "feature/a",
        )
        .expect("initial setup");

        // Lock the worktree, then delete its directory out from under git —
        // simulating an external cleanup racing the daemon's own recovery.
        g(&repo, &["worktree", "lock", rwt.to_str().unwrap()]);
        std::fs::remove_dir_all(&rwt).unwrap();
        assert!(!rwt.exists(), "precondition: rwt must not exist");

        worktree_add_or_reset(
            &Workspace::local(&repo),
            "guardian/g/wt-feature-a",
            &Workspace::local(&rwt),
            "feature/a",
        )
        .expect("state 1 + locked recovery");

        assert!(rwt.exists());
        assert_on_branch(&rwt, "guardian/g/wt-feature-a");
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
        g(
            &repo,
            &["branch", "--delete", "--force", "guardian/g/wt-feature-a"],
        );
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

    #[test]
    fn worktree_add_or_reset_prunes_after_final_checkout_failure() {
        let (base, repo, _fwt) = make_repo("final-fallback-prune");
        let rwt = base.join("rwt");
        let root = Workspace::local(&repo);
        let review_wt = Workspace::local(&rwt);
        let rev = "guardian/g/wt-feature-a";

        worktree_add_or_reset(&root, rev, &review_wt, "feature/a")
            .expect("initial review worktree");
        assert_on_branch(&rwt, rev);

        #[derive(Default)]
        struct FailCheckoutAndRemove {
            checkout_failures: usize,
            remove_failures: usize,
        }

        impl RecoveryFaults for FailCheckoutAndRemove {
            fn checkout_error(&mut self) -> Option<String> {
                self.checkout_failures += 1;
                Some(format!(
                    "injected checkout failure {}",
                    self.checkout_failures
                ))
            }

            fn remove_error(&mut self) -> Option<String> {
                self.remove_failures += 1;
                Some(format!(
                    "injected transient worktree remove failure {}",
                    self.remove_failures
                ))
            }
        }

        let mut faults = FailCheckoutAndRemove::default();
        let result =
            worktree_add_or_reset_with_faults(&root, rev, &review_wt, "feature/a", &mut faults);

        assert!(result.is_ok(), "final fallback must recover: {result:?}");
        assert_eq!(faults.checkout_failures, 2, "both checkout paths must fail");
        assert_eq!(
            faults.remove_failures, 2,
            "both worktree removals must fail"
        );
        assert_on_branch(&rwt, rev);
        let feature_head = git(&repo, &["rev-parse", "feature/a"])
            .expect("feature head")
            .trim()
            .to_string();
        let review_head = git(&repo, &["rev-parse", rev])
            .expect("review head")
            .trim()
            .to_string();
        assert_eq!(review_head, feature_head);

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
    fn branch_short_names_are_stable_across_unchanged_store_reads() {
        let store = Arc::new(Mutex::new(Store::open_in_memory().unwrap()));
        let id = store
            .lock()
            .unwrap()
            .create_guardian("r", "main", "/repo")
            .unwrap();
        for branch in [
            "improve-wasd-alpha",
            "improve-wasd-beta",
            "improve-wasd-gamma",
        ] {
            store
                .lock()
                .unwrap()
                .add_guardian_branch(&id, branch)
                .unwrap();
        }

        let ordered = store.lock().unwrap().get_guardian(&id).unwrap().branches;
        assert_eq!(
            ordered
                .iter()
                .map(|branch| branch.branch.as_str())
                .collect::<Vec<_>>(),
            vec![
                "improve-wasd-alpha",
                "improve-wasd-beta",
                "improve-wasd-gamma"
            ]
        );

        let first = branch_short_names(&store, &id, Some("/repo"));
        let second = branch_short_names(&store, &id, Some("/repo"));
        assert_eq!(first, second);
        assert_eq!(first["improve-wasd-alpha"], "improve-wasd");
        assert_eq!(first["improve-wasd-beta"], "improve-wasd-2");
        assert_eq!(first["improve-wasd-gamma"], "improve-wasd-3");
    }

    #[cfg(any())]
    #[test]
    fn stale_worktree_retirement_removes_safe_and_alerts_for_unsafe() {
        let (base, repo, _feature_worktree) = make_repo("retire-stale");
        let store = Arc::new(Mutex::new(Store::open_in_memory().unwrap()));
        let old = crate::store::now_ms() - WORKTREE_RETIREMENT_AGE_MS - 1;

        let make_review = |name: &str, branch: &str, status: &str| {
            let guard = store.lock().unwrap();
            let id = guard
                .create_guardian(name, "main", &repo.to_string_lossy())
                .unwrap();
            guard.add_guardian_branch(&id, branch).unwrap();
            let branch_id = guard.get_guardian(&id).unwrap().branches[0].id.clone();
            let wt = worktree_dir(&repo.to_string_lossy(), &id).join("wt-test");
            std::fs::create_dir_all(wt.parent().unwrap()).unwrap();
            g(
                &repo,
                &[
                    "worktree",
                    "add",
                    "-b",
                    &format!("guardian/{id}/wt-{branch}"),
                    &wt.to_string_lossy(),
                    "main",
                ],
            );
            guard
                .set_branch_review(
                    &id,
                    &branch_id,
                    &format!("guardian/{id}/wt-{branch}"),
                    &wt.to_string_lossy(),
                )
                .unwrap();
            guard
                .conn
                .execute(
                    "UPDATE guardians SET status=?1, updated_at_ms=?2 WHERE id=?3",
                    rusqlite::params![status, old, id],
                )
                .unwrap();
            (id, wt)
        };

        let (_safe_id, safe_wt) = make_review("safe", "safe", "deployed");
        let (unsafe_id, unsafe_wt) = make_review("unsafe", "unsafe", "in_review");
        let client = store.lock().unwrap().register_mailbox_client().unwrap();

        retire_stale_worktrees(&store);

        assert!(!safe_wt.exists(), "terminal review worktree should retire");
        assert!(
            unsafe_wt.exists(),
            "active review worktree must be retained"
        );
        let guard = store.lock().unwrap();
        let messages = guard
            .mailbox_messages_for_client(&client, true, None)
            .unwrap();
        assert_eq!(messages.len(), 1);
        assert!(messages[0].message.contains("please cancel it"));
        assert_eq!(
            messages[0].entity_uri.as_deref(),
            Some(format!("guardian:{unsafe_id}").as_str())
        );
        drop(guard);

        // The durable per-path claim prevents a daily sweep from spamming.
        retire_stale_worktrees(&store);
        assert_eq!(
            store
                .lock()
                .unwrap()
                .mailbox_messages_for_client(&client, true, None)
                .unwrap()
                .len(),
            1
        );
        g(
            &repo,
            &[
                "worktree",
                "remove",
                "--force",
                &unsafe_wt.to_string_lossy(),
            ],
        );
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn preflight_worktree_budget_with_limit_fails_fast_with_the_arithmetic_spelled_out() {
        let (base, repo, _fwt) = make_repo("preflight-tight");
        let root = Workspace::local(&repo);
        let wt_base = root.at(repo.join("wt-base"));
        let err = preflight_worktree_budget_with_limit(&root, &wt_base, "HEAD", 20)
            .expect_err("must fail when the budget is obviously too tight");
        assert!(err.contains("20-character"), "{err}");
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn preflight_worktree_budget_with_limit_passes_with_a_generous_limit() {
        let (base, repo, _fwt) = make_repo("preflight-loose");
        let root = Workspace::local(&repo);
        let wt_base = root.at(repo.join("wt-base"));
        preflight_worktree_budget_with_limit(&root, &wt_base, "HEAD", 4096)
            .expect("must pass with a generous budget");
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn admin_entry_name_falls_back_to_a_guardian_qualified_name_when_nothing_to_recover() {
        let dir = tmp_dir("admin-name-fresh");
        let wt = dir.join("g").join("g1").join("wt-RAL-121");
        std::fs::create_dir_all(&wt).unwrap();
        let name = admin_entry_name(&Workspace::local(&wt));
        assert_eq!(name, "g1-wt-RAL-121");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn relink_worktree_resolves_the_real_admin_name_when_two_worktrees_share_a_basename() {
        // Two different guardians can each build a worktree directory named
        // "wt-RAL-121" in their own g/g<n>/ folder. git's admin namespace
        // (.git/worktrees/<name>) is flat, so it disambiguates the second one
        // at `worktree add` time -- relink must recover THAT real name, not
        // assume the basename, or repairing worktree #2 would clobber
        // worktree #1's entry.
        let base = tmp_dir("relink-collide");
        let repo = base.join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        g(&repo, &["init", "--initial-branch", "main"]);
        std::fs::write(repo.join("base.txt"), "base\n").unwrap();
        g(&repo, &["add", "."]);
        g(&repo, &["commit", "--message", "base"]);

        let wt1 = repo
            .join(".git")
            .join(".ralphus")
            .join("g")
            .join("g1")
            .join("wt-RAL-121");
        let wt2 = repo
            .join(".git")
            .join(".ralphus")
            .join("g")
            .join("g2")
            .join("wt-RAL-121");
        std::fs::create_dir_all(wt1.parent().unwrap()).unwrap();
        std::fs::create_dir_all(wt2.parent().unwrap()).unwrap();
        g(
            &repo,
            &["worktree", "add", "-B", "b1", wt1.to_str().unwrap(), "main"],
        );
        g(
            &repo,
            &["worktree", "add", "-B", "b2", wt2.to_str().unwrap(), "main"],
        );

        let root = Workspace::local(&repo);
        let wt2_ws = Workspace::local(&wt2);

        // Learn wt2's real (git-disambiguated) admin name before breaking anything.
        let real_name = admin_entry_name(&wt2_ws);
        assert_ne!(
            real_name, "wt-RAL-121",
            "git must have disambiguated the second entry from the first"
        );

        // Snapshot wt1's own admin entry before touching wt2, so "untouched"
        // can be checked by exact before/after equality -- comparing against
        // a manually reconstructed path string is unsafe here since git
        // resolves short (8.3) path components while a plain PathBuf built
        // from `std::env::temp_dir()` does not, and on a machine whose TEMP
        // env var is itself in short form that mismatch is a false failure
        // unrelated to what this test is actually checking.
        let wt1_gitdir_path = repo
            .join(".git")
            .join("worktrees")
            .join("wt-RAL-121")
            .join("gitdir");
        let wt1_gitdir_before = std::fs::read_to_string(&wt1_gitdir_path).unwrap();

        // Simulate the broken-entry scenario relink_worktree exists to fix:
        // the admin directory is gone, but wt2's own `.git` pointer survives.
        std::fs::remove_dir_all(repo.join(".git").join("worktrees").join(&real_name)).unwrap();

        relink_worktree(&root, &wt2_ws, "b2").expect("relink");

        // wt1's own admin entry must be untouched -- a bare-basename
        // implementation would have overwritten it.
        let wt1_gitdir_after = std::fs::read_to_string(&wt1_gitdir_path).unwrap();
        assert_eq!(wt1_gitdir_after, wt1_gitdir_before);
        // wt2 must be usable again, still on its own branch.
        assert_eq!(
            git(&wt2, &["symbolic-ref", "--short", "HEAD"])
                .unwrap()
                .trim(),
            "b2"
        );

        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn cleanup_review_worktrees_does_not_destroy_a_similarly_prefixed_guardian() {
        // RAL-211 regression guard: a substring match on the short guardian id
        // ("g56" contained in "g560") must not let cleaning up guardian 56
        // destroy guardian 560's worktree.
        let base = tmp_dir("cleanup-g56-g560");
        let repo = base.join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        g(&repo, &["init", "--initial-branch", "main"]);
        std::fs::write(repo.join("base.txt"), "base\n").unwrap();
        g(&repo, &["add", "."]);
        g(&repo, &["commit", "--message", "base"]);

        let g56_id = "guardian-000000000056";
        let g560_id = "guardian-000000000560";
        let repo_str = repo.to_string_lossy().into_owned();
        let root = Workspace::local(&repo);
        let wt_base_56 = root.at(worktree_dir(&repo_str, g56_id));
        let wt_base_560 = root.at(worktree_dir(&repo_str, g560_id));

        let wt56 = wt_base_56.join("wt-feat");
        let wt560 = wt_base_560.join("wt-feat");
        std::fs::create_dir_all(wt56.root().parent().unwrap()).unwrap();
        std::fs::create_dir_all(wt560.root().parent().unwrap()).unwrap();
        g(
            &repo,
            &[
                "worktree",
                "add",
                "-B",
                "b56",
                wt56.root().to_str().unwrap(),
                "main",
            ],
        );
        g(
            &repo,
            &[
                "worktree",
                "add",
                "-B",
                "b560",
                wt560.root().to_str().unwrap(),
                "main",
            ],
        );

        cleanup_review_worktrees(&root, &wt_base_56, g56_id, "56", &[]);

        assert!(!wt56.root().exists(), "g56's own worktree must be removed");
        assert!(
            wt560.root().exists(),
            "g560's worktree must survive cleanup of g56"
        );
        assert_eq!(
            git(wt560.root(), &["symbolic-ref", "--short", "HEAD"])
                .unwrap()
                .trim(),
            "b560",
            "g560's worktree must still be a healthy, usable checkout"
        );

        let _ = std::fs::remove_dir_all(&base);
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
        git(&root, &["init", "--initial-branch", "main"]).expect("git init");
        git(&root, &["config", "user.email", "t@t.com"]).expect("git config email");
        git(&root, &["config", "user.name", "t"]).expect("git config name");
        git(&root, &["commit", "--allow-empty", "--message", "initial"]).expect("initial commit");
        git(&root, &["checkout", "-b", "feat-test"]).expect("create branch");
        std::fs::write(root.join("f.txt"), "hello\n").expect("write f.txt");
        git(&root, &["add", "f.txt"]).expect("git add");
        git(&root, &["commit", "--message", "add f"]).expect("git commit");
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
    // previous agent cell left it behind), `git rebase --onto <new_base>` fails
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
        g(&repo, &["init", "--initial-branch", "main"]);
        // `drive_rebase` below shells out through `GitVcs::exec_raw`, which
        // (correctly, for real repos) never injects an identity -- so this
        // throwaway repo needs one in its own local config, not just on the
        // `g()` helper's own per-invocation env vars, or a commit created
        // deep inside `drive_rebase` fails identity checks on a CI runner
        // with no global gitconfig.
        g(&repo, &["config", "user.name", "t"]);
        g(&repo, &["config", "user.email", "t@t"]);

        // Initial base commit — no blocker.txt yet.
        std::fs::write(repo.join("base.txt"), "base\n").unwrap();
        g(&repo, &["add", "."]);
        g(&repo, &["commit", "--message", "initial"]);
        let old_base_str = git(&repo, &["rev-parse", "HEAD"]).unwrap();
        let old_base = old_base_str.trim();

        // Feature branch adds feat.txt (NOT blocker.txt).
        g(&repo, &["checkout", "-b", "feature/a"]);
        std::fs::write(repo.join("feat.txt"), "feat\n").unwrap();
        g(&repo, &["add", "."]);
        g(&repo, &["commit", "--message", "add feature"]);
        g(&repo, &["checkout", "main"]);

        // Main moves forward and introduces blocker.txt.
        std::fs::write(repo.join("blocker.txt"), "on main\n").unwrap();
        g(&repo, &["add", "."]);
        g(&repo, &["commit", "--message", "main adds blocker.txt"]);
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
        // rebase itself, not proof gating -- NopRunner must never be called.
        let gate = ProofGate {
            scope: "nothing".to_string(),
            skip_auto_clean: false,
            is_final_branch: false,
        };
        let result = drive_rebase(
            &store,
            &guardian_id,
            &branch_id,
            &NopRunner,
            "feature/a",
            &Workspace::local(&wt),
            new_base,
            old_base,
            rev,
            &gate,
            &CancelToken::never(),
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

    /// Insert a minimal squad/task/cell row so `recompute_preliminary_summary`
    /// can resolve `branch`'s contributing cell back to its task worktree
    /// `cwd` via `cells.review_branch`.
    fn insert_done_cell(store: &Store, squad_id: &str, cwd: &Path, branch: &str) {
        store
            .conn
            .execute(
                "INSERT INTO squads (id, label, state, depends_on, created_at_ms, updated_at_ms) \
                 VALUES (?, NULL, 'done', '[]', 0, 0)",
                rusqlite::params![squad_id],
            )
            .unwrap();
        store
            .conn
            .execute(
                "INSERT INTO tasks (squad_id, idx, name, state, depends_on) \
                 VALUES (?, 0, 't', 'done', '[]')",
                rusqlite::params![squad_id],
            )
            .unwrap();
        store
            .conn
            .execute(
                "INSERT INTO cells \
                 (squad_id, task_idx, idx, sid, cwd, agent, state, depends_on, review_branch) \
                 VALUES (?, 0, 0, 's', ?, 'claude', 'done', '[]', ?)",
                rusqlite::params![squad_id, cwd.to_str().unwrap(), branch],
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
    fn a_full_merge_lands_worktrees_at_the_short_ral_211_layout_and_writes_a_readme() {
        // End-to-end: a full review build must use `.git/.ralphus/g/g<n>/...`,
        // never the legacy `.ralphus_guardian/<full-id>/...` layout, and must
        // regenerate `.git/.ralphus/README.md` documenting the mapping.
        let (base, repo, _fwt) = make_repo("short-layout-e2e");
        let store = Arc::new(Mutex::new(Store::open_in_memory().unwrap()));
        let id = {
            let guard = store.lock().unwrap();
            let id = guard
                .create_guardian("r", "main", repo.to_str().unwrap())
                .unwrap();
            guard.add_guardian_branch(&id, "feature/a").unwrap();
            id
        };
        let runner: Arc<dyn Runner> = Arc::new(CapturingRunner::new());
        run_merge(&store, runner.as_ref(), &id);

        let g = store.lock().unwrap().get_guardian(&id).unwrap();
        assert_eq!(g.status, "in_review", "{:?}", g.detail);

        let short_id = crate::short_paths::guardian_short_id(&id);
        let sep = std::path::MAIN_SEPARATOR;
        let combined = g.combined_worktree.expect("combined worktree recorded");
        assert!(
            combined.contains(&format!(".ralphus{sep}g{sep}{short_id}{sep}review")),
            "combined worktree should be at .git/.ralphus/g/{short_id}/review, got {combined}"
        );
        assert!(
            !combined.contains(".ralphus_guardian"),
            "must not use the legacy layout: {combined}"
        );
        let branch_wt = g.branches[0]
            .worktree
            .clone()
            .expect("branch worktree recorded");
        assert!(
            branch_wt.contains(&format!(".ralphus{sep}g{sep}{short_id}{sep}wt-feature")),
            "branch worktree should be under .git/.ralphus/g/{short_id}/wt-<short>, got {branch_wt}"
        );

        let readme_path = repo.join(".git").join(".ralphus").join("README.md");
        let readme = std::fs::read_to_string(&readme_path).expect("README.md must be written");
        assert!(readme.contains(&short_id), "{readme}");
        assert!(readme.contains("feature/a"), "{readme}");

        let _ = std::fs::remove_dir_all(&base);
    }

    // ── RAL-378: readable review branches ──────────────────────────────────

    #[test]
    fn a_full_merge_builds_on_a_readable_review_branch() {
        // End-to-end: the branch a review's commits land on is named after the
        // task branch, not `guardian/<id>/wt-<branch>`, and the name is
        // persisted so a later rebuild reuses it rather than re-resolving.
        let (base, repo, _fwt) = make_repo("readable-review-branch");
        let store = Arc::new(Mutex::new(Store::open_in_memory().unwrap()));
        let id = {
            let guard = store.lock().unwrap();
            let id = guard
                .create_guardian("r", "main", repo.to_str().unwrap())
                .unwrap();
            guard.add_guardian_branch(&id, "feature/a").unwrap();
            id
        };
        let runner: Arc<dyn Runner> = Arc::new(CapturingRunner::new());
        run_merge(&store, runner.as_ref(), &id);

        let g = store.lock().unwrap().get_guardian(&id).unwrap();
        assert_eq!(g.status, "in_review", "{:?}", g.detail);
        assert_eq!(
            g.branches[0].review_branch_name.as_deref(),
            Some("feature/a-review")
        );
        assert_eq!(
            g.branches[0].review_branch.as_deref(),
            Some("feature/a-review")
        );
        assert!(
            git(&repo, &["rev-parse", "--verify", "feature/a-review"]).is_ok(),
            "the readable branch must exist as a real ref"
        );
        assert!(
            git(
                &repo,
                &[
                    "rev-parse",
                    "--verify",
                    &format!("guardian/{id}/wt-feature/a")
                ]
            )
            .is_err(),
            "the internal ref must not be created for a readable branch"
        );

        // Rebuilding reuses the same name -- no walk to `-2`.
        run_merge(&store, runner.as_ref(), &id);
        let g = store.lock().unwrap().get_guardian(&id).unwrap();
        assert_eq!(
            g.branches[0].review_branch_name.as_deref(),
            Some("feature/a-review")
        );

        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn a_review_branch_name_already_taken_locally_is_suffixed() {
        // A pre-existing branch of the same name is never reset out from under
        // the user -- the review takes `-2` instead.
        let (base, repo, _fwt) = make_repo("readable-review-branch-collision");
        g(&repo, &["branch", "feature/a-review", "main"]);
        let taken_sha = git(&repo, &["rev-parse", "feature/a-review"])
            .unwrap()
            .trim()
            .to_string();
        let store = Arc::new(Mutex::new(Store::open_in_memory().unwrap()));
        let id = {
            let guard = store.lock().unwrap();
            let id = guard
                .create_guardian("r", "main", repo.to_str().unwrap())
                .unwrap();
            guard.add_guardian_branch(&id, "feature/a").unwrap();
            id
        };
        let runner: Arc<dyn Runner> = Arc::new(CapturingRunner::new());
        run_merge(&store, runner.as_ref(), &id);

        let gv = store.lock().unwrap().get_guardian(&id).unwrap();
        assert_eq!(gv.status, "in_review", "{:?}", gv.detail);
        assert_eq!(
            gv.branches[0].review_branch_name.as_deref(),
            Some("feature/a-review-2")
        );
        assert_eq!(
            git(&repo, &["rev-parse", "feature/a-review"])
                .unwrap()
                .trim(),
            taken_sha,
            "the user's own branch must be left exactly where it was"
        );

        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn a_combined_review_branch_is_named_from_the_reviews_own_name() {
        let (base, repo, _fwt) = make_repo("readable-combined-review-branch");
        let store = Arc::new(Mutex::new(Store::open_in_memory().unwrap()));
        let id = {
            let guard = store.lock().unwrap();
            let id = guard
                .create_guardian("RAL-378 Readable Branches", "main", repo.to_str().unwrap())
                .unwrap();
            guard.add_guardian_branch(&id, "feature/a").unwrap();
            id
        };
        let runner: Arc<dyn Runner> = Arc::new(CapturingRunner::new());
        run_merge(&store, runner.as_ref(), &id);

        let gv = store.lock().unwrap().get_guardian(&id).unwrap();
        assert_eq!(gv.status, "in_review", "{:?}", gv.detail);
        assert_eq!(
            gv.review_branch.as_deref(),
            Some("ral-378-readable-branches-review")
        );
        // Renaming the review afterwards must not move the branch.
        store
            .lock()
            .unwrap()
            .rename_guardian(&id, "Something Else Entirely")
            .unwrap();
        run_merge(&store, runner.as_ref(), &id);
        let gv = store.lock().unwrap().get_guardian(&id).unwrap();
        assert_eq!(
            gv.review_branch_name.as_deref(),
            Some("ral-378-readable-branches-review")
        );

        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn a_merge_succeeds_alongside_an_untouched_old_layout_leftover() {
        // An abandoned old-layout `.ralphus_guardian/<full-id>` directory
        // must not block or be migrated by a fresh merge under the
        // `.ralphus/g/g<n>` layout.
        let (base, repo, _fwt) = make_repo("old-layout-coexist-guardian");
        let store = Arc::new(Mutex::new(Store::open_in_memory().unwrap()));
        let id = {
            let guard = store.lock().unwrap();
            let id = guard
                .create_guardian("r", "main", repo.to_str().unwrap())
                .unwrap();
            guard.add_guardian_branch(&id, "feature/a").unwrap();
            id
        };
        let old_dir = repo.join(".git").join(".ralphus_guardian").join(&id);
        std::fs::create_dir_all(&old_dir).unwrap();
        std::fs::write(old_dir.join("leftover.txt"), "abandoned\n").unwrap();

        let runner: Arc<dyn Runner> = Arc::new(CapturingRunner::new());
        run_merge(&store, runner.as_ref(), &id);

        let g = store.lock().unwrap().get_guardian(&id).unwrap();
        assert_eq!(
            g.status, "in_review",
            "an old-layout leftover must not block a new merge: {:?}",
            g.detail
        );
        assert!(
            old_dir.join("leftover.txt").exists(),
            "old-layout leftovers must be left alone, not migrated or purged"
        );

        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    #[cfg_attr(not(windows), ignore = "MAX_PATH is a Windows-specific limit")]
    fn a_full_merge_completes_from_a_repository_root_long_enough_to_have_failed_pre_ral_211() {
        // A repo root deep enough that a `.ralphus_guardian/<full-id>/
        // wt-<branch>` overhead (77-87 chars) would push even a short
        // tracked file past Windows' 260-char MAX_PATH, but the current
        // `.ralphus/g/g<n>/wt-<short>` overhead (~31 chars) fits.
        // `make_repo`'s `tag` becomes part of the temp-dir name itself, so
        // padding it directly inflates the repo root's path length -- grown
        // in a small calibration loop so this is robust to how long the
        // host's own temp directory happens to be.
        let mut pad_len = 120usize;
        let (base, repo, _fwt) = loop {
            let (base, repo, fwt) = make_repo(&"x".repeat(pad_len));
            let root_len = repo.to_string_lossy().len();
            if root_len + 87 > 260 && root_len + 31 <= 260 {
                break (base, repo, fwt);
            }
            let _ = std::fs::remove_dir_all(&base);
            assert!(
                pad_len < 400,
                "could not reach the target path-length window"
            );
            pad_len += 40;
        };

        let store = Arc::new(Mutex::new(Store::open_in_memory().unwrap()));
        let id = {
            let guard = store.lock().unwrap();
            let id = guard
                .create_guardian("r", "main", repo.to_str().unwrap())
                .unwrap();
            guard.add_guardian_branch(&id, "feature/a").unwrap();
            id
        };
        let runner: Arc<dyn Runner> = Arc::new(CapturingRunner::new());
        run_merge(&store, runner.as_ref(), &id);

        let g = store.lock().unwrap().get_guardian(&id).unwrap();
        assert_eq!(
            g.status, "in_review",
            "a repo root in the pre-RAL-211 failure zone must still merge successfully: {:?}",
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
        assert!(b.source_cell_machine.is_none());
        assert!(
            fetch_branch_for_remote_cell(&store, &id, &b).is_ok(),
            "a local branch must not attempt any fetch"
        );

        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn a_remote_branch_that_was_never_pushed_fails_with_an_actionable_message() {
        // The quietly-wrong case Q6 exists for: the task ran on another machine
        // and never pushed, so its commits are nowhere this repo can see them.
        // Failing here beats an opaque `worktree add` error -- or, when a
        // previous squad *did* push, silently stacking that stale revision.
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
        b.source_cell_machine = Some("incredibuild:A".to_string());
        // `make_repo` configures no remote, so the fetch cannot succeed --
        // exactly what an unpushed branch looks like from here.
        let err = fetch_branch_for_remote_cell(&store, &id, &b)
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
    fn record_feedback_reply_is_best_effort_and_leaves_thread_untouched_on_failure() {
        // RAL-272: an agent unsupported by the direct-LLM-call fast path
        // (`chat_client::call_direct` only supports claude/anthropic/ollama)
        // must not panic or leak an error into the branch's feedback thread
        // -- the reply generation is best-effort only, with no subprocess
        // fallback. "claude-code" is resolvable but unsupported, so this is
        // deterministic and network-free regardless of whether
        // ANTHROPIC_API_KEY is set in the environment.
        let store = Arc::new(Mutex::new(Store::open_in_memory().unwrap()));
        let id = {
            let g = store.lock().unwrap();
            let id = g.create_guardian("r", "main", "/repo").unwrap();
            g.add_guardian_branch(&id, "feature/a").unwrap();
            g.set_guardian_resolver(&id, Some("claude-code"), None)
                .unwrap();
            id
        };
        let branch_id = store.lock().unwrap().get_guardian(&id).unwrap().branches[0]
            .id
            .clone();
        record_feedback_reply(
            &store,
            &id,
            &branch_id,
            "feature/a",
            "please fix the naming",
        );
        let msgs = store
            .lock()
            .unwrap()
            .guardian_branch_messages(&id, &branch_id)
            .unwrap();
        assert!(
            msgs.is_empty(),
            "an unsupported resolver agent must not record a reply: {msgs:?}"
        );
    }

    #[test]
    fn note_if_branch_is_empty_flags_a_branch_that_adds_nothing() {
        // The quietly-wrong case this exists for: a task whose cell never
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
            insert_done_cell(&guard, "squad-1", &fwt, "feature/a");
            id
        };
        let branch_id = store.lock().unwrap().get_guardian(&id).unwrap().branches[0]
            .id
            .clone();

        // A branch still `pending` (no cell done yet) contributes nothing.
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

        // Cell done -> mark the branch Ready (as the scheduler now does
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
        // `generate_final_summary` last wrote untouched.
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
        g(&awt, &["commit", "--message", "commit-a-only"]);

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
        g(&bwt, &["commit", "--message", "commit-b-only"]);

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
        g(&cwt, &["commit", "--message", "commit-c-only"]);

        let store = Arc::new(Mutex::new(Store::open_in_memory().unwrap()));
        let id = {
            let guard = store.lock().unwrap();
            let id = guard
                .create_guardian("r", "main", repo.to_str().unwrap())
                .unwrap();
            for branch in ["feature/a", "feature/b", "feature/c"] {
                guard.add_guardian_branch(&id, branch).unwrap();
            }
            insert_done_cell(&guard, "squad-a", &awt, "feature/a");
            insert_done_cell(&guard, "squad-b", &bwt, "feature/b");
            insert_done_cell(&guard, "squad-c", &cwt, "feature/c");
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

    /// RAL-303: a partly-rebased stack must still produce one section per
    /// enabled branch. Branch A has been rebased into the review (so its
    /// commits live on `guardian/<id>/wt-feature/a`, not in any un-reviewed
    /// worktree) while branch B has not; the summary previously dropped every
    /// rebased branch, so a seven-branch review whose newest branch was the
    /// only un-rebased one ended up describing that one branch alone.
    #[test]
    fn recompute_preliminary_summary_covers_already_rebased_branches() {
        let (base, repo, awt) = make_repo("prelim-rebased");
        let base_sha = git(&repo, &["rev-parse", "main"])
            .unwrap()
            .trim()
            .to_string();

        // feature/b is stacked on feature/a, exactly as the real merge flow
        // leaves it.
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
        g(&bwt, &["commit", "--message", "commit-b-only"]);

        let store = Arc::new(Mutex::new(Store::open_in_memory().unwrap()));
        let id = {
            let guard = store.lock().unwrap();
            let id = guard
                .create_guardian("r", "main", repo.to_str().unwrap())
                .unwrap();
            guard.add_guardian_branch(&id, "feature/a").unwrap();
            guard.add_guardian_branch(&id, "feature/b").unwrap();
            insert_done_cell(&guard, "squad-a", &awt, "feature/a");
            insert_done_cell(&guard, "squad-b", &bwt, "feature/b");
            guard
                .set_guardian_project_base_commit(&id, repo.to_str().unwrap(), &base_sha)
                .unwrap();
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

        // feature/a has been rebased into the review: its review ref exists
        // and its BranchView carries a worktree.
        let review_ref = format!("guardian/{id}/wt-feature/a");
        g(&repo, &["update-ref", &review_ref, "feature/a"]);
        {
            let guard = store.lock().unwrap();
            guard
                .set_branch_review(&id, &branch_ids[0], &review_ref, "/some/review/wt")
                .unwrap();
            for branch_id in &branch_ids {
                guard
                    .set_branch_status(&id, branch_id, MergeStatus::Ready, None)
                    .unwrap();
            }
        }

        recompute_preliminary_summary(&store, &id);
        let summary = store
            .lock()
            .unwrap()
            .get_guardian(&id)
            .unwrap()
            .change_summary
            .expect("preliminary summary computed");

        let sections: std::collections::HashMap<&str, &str> = summary
            .split("\n\n")
            .filter_map(|s| s.split_once(":\n"))
            .collect();
        // feature/a is read from its review ref, feature/b from its own
        // worktree -- and feature/b still reports only its own commit.
        assert_eq!(sections.get("feature/a"), Some(&"feature"));
        assert_eq!(sections.get("feature/b"), Some(&"commit-b-only"));

        let _ = std::fs::remove_dir_all(&base);
    }

    /// RAL-303: a disabled branch is not part of the review, so none of its
    /// commits may reach the change summary — and disabling it must not leak
    /// its commits into the next branch's section either, since the running
    /// base skips straight over it.
    #[test]
    fn recompute_preliminary_summary_skips_disabled_branches() {
        let (base, repo, awt) = make_repo("prelim-disabled");

        // feature/b stacks on feature/a, feature/c stacks on feature/b.
        let mut prev = "feature/a".to_string();
        let mut worktrees = vec![("feature/a".to_string(), awt)];
        for name in ["feature/b", "feature/c"] {
            let wt = base.join(name.replace('/', "-"));
            g(
                &repo,
                &["worktree", "add", "-b", name, wt.to_str().unwrap(), &prev],
            );
            std::fs::write(wt.join(format!("{}.txt", name.replace('/', "-"))), "x\n").unwrap();
            g(&wt, &["add", "."]);
            g(
                &wt,
                &["commit", "--message", &format!("commit-{name}-only")],
            );
            worktrees.push((name.to_string(), wt));
            prev = name.to_string();
        }

        let store = Arc::new(Mutex::new(Store::open_in_memory().unwrap()));
        let id = {
            let guard = store.lock().unwrap();
            let id = guard
                .create_guardian("r", "main", repo.to_str().unwrap())
                .unwrap();
            for (i, (branch, wt)) in worktrees.iter().enumerate() {
                guard.add_guardian_branch(&id, branch).unwrap();
                insert_done_cell(&guard, &format!("squad-{i}"), wt, branch);
            }
            id
        };
        {
            let guard = store.lock().unwrap();
            for b in &guard.get_guardian(&id).unwrap().branches {
                guard
                    .set_branch_status(&id, &b.id, MergeStatus::Ready, None)
                    .unwrap();
            }
            guard
                .set_branch_enabled_by_name(&id, "feature/b", false)
                .unwrap();
        }

        recompute_preliminary_summary(&store, &id);
        let summary = store
            .lock()
            .unwrap()
            .get_guardian(&id)
            .unwrap()
            .change_summary
            .expect("preliminary summary computed");

        assert!(
            !summary.contains("feature/b"),
            "disabled branch must not appear at all: {summary:?}"
        );
        assert!(
            !summary.contains("commit-feature/b-only"),
            "a disabled branch's commits must not leak into a sibling: {summary:?}"
        );
        let sections: std::collections::HashMap<&str, &str> = summary
            .split("\n\n")
            .filter_map(|s| s.split_once(":\n"))
            .collect();
        assert_eq!(sections.get("feature/a"), Some(&"feature"));
        assert_eq!(sections.get("feature/c"), Some(&"commit-feature/c-only"));

        let _ = std::fs::remove_dir_all(&base);
    }

    /// RAL-303: a rebased branch's review ref is a *copy* of the producing
    /// worktree's commits under different shas, so a partly-rebased stack
    /// reads the same commit twice — once by its review sha, once by its
    /// original. Assert the summary lists each commit exactly once, attributed
    /// to the earliest branch that carries it, even when the branch beneath is
    /// read from a side the range-chaining can't bridge (its producing
    /// worktree is no longer known to the store).
    #[test]
    fn recompute_preliminary_summary_strips_commits_duplicated_across_the_rebase_seam() {
        let (base, repo, _awt) = make_repo("prelim-dupes");
        let base_sha = git(&repo, &["rev-parse", "main"])
            .unwrap()
            .trim()
            .to_string();

        // feature/b is stacked on feature/a, so its worktree carries feature/a's
        // "feature" commit as well as its own.
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
        g(&bwt, &["commit", "--message", "commit-b-only"]);

        let store = Arc::new(Mutex::new(Store::open_in_memory().unwrap()));
        let id = {
            let guard = store.lock().unwrap();
            let id = guard
                .create_guardian("r", "main", repo.to_str().unwrap())
                .unwrap();
            guard.add_guardian_branch(&id, "feature/a").unwrap();
            guard.add_guardian_branch(&id, "feature/b").unwrap();
            // Only feature/b has a producing cell on record: feature/a is
            // readable solely through its review ref, so the task-side chain
            // has no HEAD to advance past and would otherwise re-list
            // feature/a's commit under feature/b.
            insert_done_cell(&guard, "squad-b", &bwt, "feature/b");
            guard
                .set_guardian_project_base_commit(&id, repo.to_str().unwrap(), &base_sha)
                .unwrap();
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

        let review_ref = format!("guardian/{id}/wt-feature/a");
        g(&repo, &["update-ref", &review_ref, "feature/a"]);
        {
            let guard = store.lock().unwrap();
            guard
                .set_branch_review(&id, &branch_ids[0], &review_ref, "/some/review/wt")
                .unwrap();
            for branch_id in &branch_ids {
                guard
                    .set_branch_status(&id, branch_id, MergeStatus::Ready, None)
                    .unwrap();
            }
        }

        recompute_preliminary_summary(&store, &id);
        let summary = store
            .lock()
            .unwrap()
            .get_guardian(&id)
            .unwrap()
            .change_summary
            .expect("preliminary summary computed");

        assert_eq!(
            summary.lines().filter(|l| *l == "feature").count(),
            1,
            "the shared commit must appear once, under feature/a: {summary:?}"
        );
        let sections: std::collections::HashMap<&str, &str> = summary
            .split("\n\n")
            .filter_map(|s| s.split_once(":\n"))
            .collect();
        assert_eq!(sections.get("feature/a"), Some(&"feature"));
        assert_eq!(sections.get("feature/b"), Some(&"commit-b-only"));

        let _ = std::fs::remove_dir_all(&base);
    }

    /// RAL-303: once every enabled branch is in the review worktree there is
    /// nothing the git-log pass can add, so it must not overwrite the
    /// agent-authored summary with its own raw commit subjects.
    #[test]
    fn recompute_preliminary_summary_never_downgrades_a_final_summary() {
        let (base, repo, awt) = make_repo("prelim-no-downgrade");
        let store = Arc::new(Mutex::new(Store::open_in_memory().unwrap()));
        let id = {
            let guard = store.lock().unwrap();
            let id = guard
                .create_guardian("r", "main", repo.to_str().unwrap())
                .unwrap();
            guard.add_guardian_branch(&id, "feature/a").unwrap();
            insert_done_cell(&guard, "squad-a", &awt, "feature/a");
            id
        };
        let branch_id = store.lock().unwrap().get_guardian(&id).unwrap().branches[0]
            .id
            .clone();
        {
            let guard = store.lock().unwrap();
            guard
                .set_branch_status(&id, &branch_id, MergeStatus::Ready, None)
                .unwrap();
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
        let g_row = store.lock().unwrap().get_guardian(&id).unwrap();
        assert_eq!(
            g_row.change_summary.as_deref(),
            Some("final summary from the agent")
        );
        assert_eq!(g_row.summary_agent.as_deref(), Some("ollama"));

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
                cache_creation_tokens: 0,
                cache_read_tokens: 0,
                compaction_input_tokens: 0,
                compaction_count: 0,
                cost_usd: 0.0,
                cost_is_estimated: false,
                summary: "captured".to_string(),
                error: None,
                proofed: None,
                agent_session_id: None,
                ghost: None,
            }
        }
    }

    /// RAL-208: set up a single-branch, single-project guardian with
    /// everything [`generate_final_summary`] needs to find real content: a
    /// base commit recorded via `set_guardian_project_base_commit`, and a
    /// `guardian/<id>/wt-<branch>` ref (what a real merge's per-branch
    /// worktree creation leaves behind) pointing at `tip`. Returns the
    /// guardian id.
    fn setup_final_summary_guardian(
        store: &Arc<Mutex<Store>>,
        repo: &Path,
        base_sha: &str,
        branch: &str,
        tip: &str,
    ) -> String {
        let id = {
            let guard = store.lock().unwrap();
            let id = guard
                .create_guardian("r", "main", repo.to_str().unwrap())
                .unwrap();
            guard.add_guardian_branch(&id, branch).unwrap();
            guard
                .set_guardian_project_base_commit(&id, repo.to_str().unwrap(), base_sha)
                .unwrap();
            // Final-summary generation only runs on an InReview guardian;
            // the merge pass that would normally reach it is the part these
            // tests stub out, so land the state the same way the merge pass
            // ends.
            guard
                .set_guardian_status(&id, GuardianStatus::InReview, None)
                .unwrap();
            id
        };
        g(
            repo,
            &[
                "branch",
                "--force",
                &format!("guardian/{id}/wt-{branch}"),
                tip,
            ],
        );
        id
    }

    #[test]
    fn generate_final_summary_default_prompt_requests_bullet_list() {
        let (base, repo, _fwt) = make_repo("gensum-bullet");
        let base_sha = git(&repo, &["rev-parse", "main"])
            .unwrap()
            .trim()
            .to_string();
        let store = Arc::new(Mutex::new(Store::open_in_memory().unwrap()));
        let id = setup_final_summary_guardian(&store, &repo, &base_sha, "feature/a", "feature/a");
        let runner = CapturingRunner::new();

        generate_final_summary(&store, &runner, &id, "sig");

        let prompt = runner
            .last_prompt
            .lock()
            .unwrap()
            .clone()
            .expect("prompt captured");
        assert!(prompt.contains("bullet list"), "prompt: {prompt}");
        assert!(prompt.contains("80 characters"), "prompt: {prompt}");
        assert!(prompt.contains("feature/a"), "prompt: {prompt}");

        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn generate_final_summary_prose_config_disables_bullet_list() {
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
        let id = setup_final_summary_guardian(&store, &repo, &base_sha, "feature/a", "feature/a");
        let runner = CapturingRunner::new();

        generate_final_summary(&store, &runner, &id, "sig");

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
    fn generate_final_summary_bullet_prompt_uses_ticket_label_not_raw_branch_name() {
        let (base, repo, _fwt) = make_repo("gensum-label");
        let base_sha = git(&repo, &["rev-parse", "main"])
            .unwrap()
            .trim()
            .to_string();
        let store = Arc::new(Mutex::new(Store::open_in_memory().unwrap()));
        // The branch's own review-worktree ref points at feature/a's real
        // tip -- only the guardian branch's *name* is ticket-shaped, so this
        // isolates label substitution from git-log resolution.
        let id = setup_final_summary_guardian(
            &store,
            &repo,
            &base_sha,
            "RAL-124-bullet_change_summary",
            "feature/a",
        );
        let runner = CapturingRunner::new();

        generate_final_summary(&store, &runner, &id, "sig");

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

    #[test]
    fn generate_final_summary_marks_signature_generated_on_success() {
        let (base, repo, _fwt) = make_repo("gensum-mark");
        let base_sha = git(&repo, &["rev-parse", "main"])
            .unwrap()
            .trim()
            .to_string();
        let store = Arc::new(Mutex::new(Store::open_in_memory().unwrap()));
        let id = setup_final_summary_guardian(&store, &repo, &base_sha, "feature/a", "feature/a");
        let runner = CapturingRunner::new();

        generate_final_summary(&store, &runner, &id, "sig-1");

        let g = store.lock().unwrap().get_guardian(&id).unwrap();
        assert_eq!(g.change_summary.as_deref(), Some("captured"));

        // A second request for the SAME signature is now recognised as
        // already-satisfied and stays a no-op.
        store
            .lock()
            .unwrap()
            .request_final_summary(&id, "sig-1", crate::store::now_ms(), false);
        assert!(
            store
                .lock()
                .unwrap()
                .take_due_final_summary_requests(crate::store::now_ms() + 60_000, 0)
                .is_empty()
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
        g(&repo, &["init", "--initial-branch", "main"]);
        std::fs::write(repo.join("shared.txt"), "base\n").unwrap();
        g(&repo, &["add", "."]);
        g(&repo, &["commit", "--message", "base"]);

        // Feature branch changes shared.txt one way...
        g(&repo, &["checkout", "-b", "feature/a"]);
        std::fs::write(repo.join("shared.txt"), "feature\n").unwrap();
        g(&repo, &["add", "."]);
        g(&repo, &["commit", "--message", "feature change"]);

        // ...while main changes it another way, so rebasing feature/a onto
        // main conflicts on shared.txt.
        g(&repo, &["checkout", "main"]);
        std::fs::write(repo.join("shared.txt"), "main\n").unwrap();
        g(&repo, &["add", "."]);
        g(&repo, &["commit", "--message", "main change"]);

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
            .env("GIT_AUTHOR_NAME", "t")
            .env("GIT_AUTHOR_EMAIL", "t@t")
            .env("GIT_COMMITTER_NAME", "t")
            .env("GIT_COMMITTER_EMAIL", "t@t")
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
            &FixedValueRunner("unused"),
            &id,
            &Workspace::local(&repo),
            &repo.to_string_lossy(),
            &CancelToken::never(),
        );
        // The configured `exit 1` check gate fails the review.
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("check failed"));

        let _ = std::fs::remove_dir_all(&base);
    }

    /// RAL-203: `final_checks` (the finalize-time build/check-gate step
    /// against the combined worktree) runs under this review's own
    /// `build_env` -- the check command below fails unless the overridden
    /// variable is actually present in its process environment.
    #[test]
    fn final_checks_runs_check_gates_under_this_reviews_build_env_override() {
        let (base, repo, _fwt) = make_repo("finalchecks-buildenv");
        let store = Arc::new(Mutex::new(Store::open_in_memory().unwrap()));
        let check_cmd = if cfg!(windows) {
            "if not \"%RAL203_BUILD_VAR%\"==\"expected\" exit 1"
        } else {
            "test \"$RAL203_BUILD_VAR\" = expected"
        };
        let id = {
            let guard = store.lock().unwrap();
            let id = guard
                .create_guardian("r", "main", &repo.to_string_lossy())
                .unwrap();
            guard
                .set_guardian_checks(&id, &[check_cmd.to_string()])
                .unwrap();
            let mut set = std::collections::BTreeMap::new();
            set.insert("RAL203_BUILD_VAR".to_string(), "expected".to_string());
            guard
                .set_guardian_build_env_overrides(&id, &set, &[], &[])
                .unwrap();
            id
        };

        let result = final_checks(
            &store,
            &FixedValueRunner("unused"),
            &id,
            &Workspace::local(&repo),
            &repo.to_string_lossy(),
            &CancelToken::never(),
        );
        assert!(
            result.is_ok(),
            "check gate must see the build-env override: {result:?}"
        );

        let _ = std::fs::remove_dir_all(&base);
    }

    /// RAL-203/RAL-313: `final_checks`'s project `auto_build` fallback
    /// (RAL-101, taken when a review declares no explicit `checks`) shares
    /// the same `env` fetch as the check-gates branch above -- confirm it
    /// also runs under this review's own `build_env`.
    #[test]
    fn final_checks_runs_project_auto_build_under_this_reviews_build_env_override() {
        let (base, repo, _fwt) = make_repo("finalchecks-autobuild-buildenv");
        let build_cmd = if cfg!(windows) {
            "if not \"%RAL313_AUTOBUILD_VAR%\"==\"expected\" exit 1"
        } else {
            "test \"$RAL313_AUTOBUILD_VAR\" = expected"
        };
        std::fs::write(
            repo.join(".ralphus.toml"),
            format!("[review]\nauto_build = {build_cmd:?}\n"),
        )
        .unwrap();
        let store = Arc::new(Mutex::new(Store::open_in_memory().unwrap()));
        let id = {
            let guard = store.lock().unwrap();
            let id = guard
                .create_guardian("r", "main", &repo.to_string_lossy())
                .unwrap();
            let mut set = std::collections::BTreeMap::new();
            set.insert("RAL313_AUTOBUILD_VAR".to_string(), "expected".to_string());
            guard
                .set_guardian_build_env_overrides(&id, &set, &[], &[])
                .unwrap();
            id
        };

        let result = final_checks(
            &store,
            &FixedValueRunner("unused"),
            &id,
            &Workspace::local(&repo),
            &repo.to_string_lossy(),
            &CancelToken::never(),
        );
        assert!(
            result.is_ok(),
            "project auto_build must see the build-env override: {result:?}"
        );
        assert!(
            result
                .unwrap()
                .unwrap()
                .contains("auto-built via project default"),
            "expected the project-auto_build note"
        );

        let _ = std::fs::remove_dir_all(&base);
    }

    /// RAL-342: a review's own declared `[[review.auto_build]]` command runs
    /// ahead of (and instead of) the project-level `.ralphus.toml [review]
    /// auto_build` default when both are configured. The project default is
    /// set to a command that would fail, so if it ran instead of the
    /// review-declared one, this test would fail.
    #[test]
    fn final_checks_prefers_review_declared_auto_build_over_project_default() {
        let (base, repo, _fwt) = make_repo("finalchecks-review-autobuild-precedence");
        std::fs::write(
            repo.join(".ralphus.toml"),
            "[review]\nauto_build = \"exit 1\"\n",
        )
        .unwrap();
        let store = Arc::new(Mutex::new(Store::open_in_memory().unwrap()));
        let id = {
            let guard = store.lock().unwrap();
            let id = guard
                .create_guardian("r", "main", &repo.to_string_lossy())
                .unwrap();
            guard
                .set_guardian_auto_build(
                    &id,
                    Some(&crate::guardian::GuardianAutoBuild {
                        command: Some("exit 0".to_string()),
                        prompt: None,
                        system_prompt: None,
                        system_prompt_position: None,
                        agent: None,
                        model: None,
                    }),
                )
                .unwrap();
            id
        };

        let result = final_checks(
            &store,
            &FixedValueRunner("unused"),
            &id,
            &Workspace::local(&repo),
            &repo.to_string_lossy(),
            &CancelToken::never(),
        );
        assert!(
            result.is_ok(),
            "the review-declared auto_build must win and succeed: {result:?}"
        );
        let note = result.unwrap().unwrap();
        assert!(
            note.contains("auto-built via review auto_build"),
            "expected the review-declared auto_build note, got: {note}"
        );

        let _ = std::fs::remove_dir_all(&base);
    }

    struct FailingAgentRunner {
        cost_usd: f64,
    }

    impl Runner for FailingAgentRunner {
        fn run(&self, _spec: &RunnerSpec) -> RunnerResult {
            RunnerResult {
                status: "failed".to_string(),
                tokens_in: 10,
                tokens_out: 20,
                cache_creation_tokens: 0,
                cache_read_tokens: 0,
                compaction_input_tokens: 0,
                compaction_count: 0,
                cost_usd: self.cost_usd,
                cost_is_estimated: false,
                summary: String::new(),
                error: Some("boom: agent exploded".to_string()),
                proofed: None,
                agent_session_id: None,
                ghost: None,
            }
        }
    }

    /// RAL-342/Q5: a review-declared `[[review.auto_build]]` *agent* invocation
    /// that fails must not fail the merge -- it surfaces as an advisory
    /// notice plus a Cartographer log entry, and `final_checks` still returns
    /// `Ok(Some(note))` (never `Err`) so the review reaches `InReview`. Cost
    /// is still recorded via `record_guardian_call_cost` even though the call
    /// failed.
    #[test]
    fn final_checks_review_auto_build_agent_failure_is_advisory_not_err() {
        let (base, repo, _fwt) = make_repo("finalchecks-review-autobuild-agent-fail");
        let store = Arc::new(Mutex::new(Store::open_in_memory().unwrap()));
        let id = {
            let guard = store.lock().unwrap();
            let id = guard
                .create_guardian("r", "main", &repo.to_string_lossy())
                .unwrap();
            guard
                .set_guardian_auto_build(
                    &id,
                    Some(&crate::guardian::GuardianAutoBuild {
                        command: None,
                        prompt: Some("build the thing".to_string()),
                        system_prompt: None,
                        system_prompt_position: None,
                        agent: None,
                        model: None,
                    }),
                )
                .unwrap();
            id
        };

        let result = final_checks(
            &store,
            &FailingAgentRunner { cost_usd: 1.25 },
            &id,
            &Workspace::local(&repo),
            &repo.to_string_lossy(),
            &CancelToken::never(),
        );
        assert!(
            result.is_ok(),
            "an agent-form auto_build failure must be advisory, not an Err: {result:?}"
        );
        let note = result.unwrap().unwrap();
        assert!(
            note.contains("failed"),
            "expected the failure to be reflected in the note: {note}"
        );

        let guard = store.lock().unwrap();
        let view = guard.get_guardian(&id).unwrap();
        assert_eq!(view.notice_kind.as_deref(), Some("auto_build_failed"));
        assert!(
            view.notice_message
                .as_deref()
                .unwrap()
                .contains("boom: agent exploded"),
            "notice: {:?}",
            view.notice_message
        );

        let (_, _, cumulative) = guard.guardian_cost_total(&id).unwrap();
        assert!(
            (cumulative - 1.25).abs() < 1e-9,
            "cost must still be recorded on a failed auto_build agent call: {cumulative}"
        );

        let page = guard
            .cartographer_query(&crate::cartographer::CartographerFilter {
                guardian_id: Some(id.clone()),
                ..crate::cartographer::CartographerFilter::recent(10)
            })
            .unwrap();
        assert!(
            page.rows
                .iter()
                .any(|r| r.message == "review auto_build (agent) failed"),
            "expected a Cartographer entry for the failed agent auto_build"
        );

        let _ = std::fs::remove_dir_all(&base);
    }
}
