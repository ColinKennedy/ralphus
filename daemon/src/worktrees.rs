//! Placeholder `cwd` resolution + git worktree materialization (RAL-100).
//!
//! A cell `cwd` of the form `ralphus:new-worktree/<branch>?upstream=<upstream>`
//! names a branch to check out in a dedicated worktree under
//! `.git/.ralphus/w/<short>` (`<short>` is a truncated form of `branch`; see
//! `crate::short_paths`. Git branch names are never shortened, only this
//! directory name), rather than a real filesystem path. The project to
//! materialize it under is NOT embedded in the `cwd` string -- it's the
//! owning task's `project` field (required whenever any of its cells uses
//! this placeholder). The `?upstream=<upstream>` suffix is REQUIRED too:
//! submit-time validation (`ralphus_core::validate`) rejects a placeholder
//! cwd with none, and [`ensure_worktree`] always applies it via
//! `git branch --set-upstream-to`, so a freshly materialized branch's
//! tracking target is always the author's explicit choice, never a guess
//! from whatever `HEAD` happened to be at materialization time. Two reserved
//! `?upstream=<<...>>` sentinels (RAL-258) let the author defer that choice:
//! `<<default>>` (the repository's default branch, recommended) and
//! `<<current_branch>>` (whatever branch the project currently has checked
//! out). [`resolve_placeholders`] expands these against the project root via
//! [`resolve_upstream`] before materialization.
//! [`resolve_placeholders`] resolves every placeholder among a squad's cells
//! exactly once (memoized by the literal placeholder string, so the same
//! value repeated across cells/tasks only materializes one worktree),
//! rewriting each cell's stored `cwd` to the real resolved path.
//!
//! Restart safety falls out of that rewrite: once a cell's `cwd` has been
//! resolved and persisted, a restarted squad reads the real path back from the
//! store — the placeholder string is gone; there's nothing left to resolve.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};

use opentelemetry::Context;
use opentelemetry::trace::{SpanKind, Status};

use crate::guardian_merge::git;
use crate::otel;
use crate::store::{CellRow, ProjectView, Store, TaskRow};

#[derive(Debug, Clone, PartialEq, Eq)]
enum BranchMaterialization {
    ExistingLocal,
    NewFromRemote { remote_ref: String },
    NewFromHead { warning: Option<String> },
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct PlaceholderContext<'a> {
    pub(crate) squad_id: &'a str,
    pub(crate) task_idx: i64,
    pub(crate) cell_idx: i64,
    pub(crate) task_name: &'a str,
    pub(crate) cell_id: &'a str,
    pub(crate) machine: Option<&'a str>,
}

trait ProjectStartupAdapter {
    fn resolve_placeholder(
        &self,
        store: &Store,
        project: &ProjectView,
        placeholder: &str,
        ctx: PlaceholderContext<'_>,
    ) -> Result<Option<String>, String>;
}

struct GitProjectStartupAdapter;

/// The on-disk worktree directory for `branch` under a project's `root`:
/// `.git/.ralphus/w/<short>`, `<short>` a truncated form of `branch` (see
/// `crate::short_paths`) so a deeply nested `root` still fits inside
/// Windows' `MAX_PATH`; the git branch itself keeps its full name.
///
/// Collision-*unaware*, like [`crate::short_paths::short_name`] itself: two
/// branches that truncate to the same short name compute the same directory
/// here. Only [`ensure_worktree`] (via [`resolve_task_worktree_dir`]) is
/// safe to use for actually materializing a worktree; this function stays a
/// pure, git-free helper for tests and for spots that just need "the
/// directory a branch would land in absent any collision."
#[must_use]
pub fn worktree_dir(root: &Path, branch: &str) -> PathBuf {
    crate::short_paths::ralphus_root(root)
        .join("w")
        .join(crate::short_paths::short_name(branch))
}

/// The `<short>` component of `path`, if `path` is a direct child of some
/// `.git/.ralphus/w/` directory -- found by matching that contiguous
/// component *run* rather than comparing `path.parent()` against a
/// separately-built `PathBuf` for equality, since `git worktree list
/// --porcelain` prints forward-slash paths even on Windows while a
/// `PathBuf` built with `.join()` uses `\`; two `Path`s naming the same
/// directory but spelled with different separators are not `==` to each
/// other (same class of mismatch [`crate::short_paths::worktree_belongs_to_guardian`]
/// already guards against, for a different underlying cause).
fn short_name_under_w(path: &Path) -> Option<String> {
    let comps: Vec<String> = path
        .components()
        .map(|c| c.as_os_str().to_string_lossy().into_owned())
        .collect();
    comps
        .windows(4)
        .find(|w| w[0] == ".git" && w[1] == ".ralphus" && w[2] == "w")
        .map(|w| w[3].clone())
}

/// Every task-worktree short name currently in use under `root`'s `w/`
/// directory, mapped to the branch it's checked out on -- read straight from
/// `git worktree list --porcelain` (the same technique
/// `guardian_merge::find_worktree_for_branch` uses), never from `w/`'s
/// directory listing, so a worktree git considers prunable/stale still
/// counts as occupying its slot until git itself says otherwise.
fn existing_task_worktree_branches(root: &Path) -> HashMap<String, String> {
    let mut out = HashMap::new();
    let Ok(list) = git(root, &["worktree", "list", "--porcelain"]) else {
        return out;
    };
    let mut cur_short: Option<String> = None;
    for line in list.lines() {
        if let Some(path) = line.strip_prefix("worktree ") {
            cur_short = short_name_under_w(Path::new(path.trim()));
        } else if let Some(branch) = line.strip_prefix("branch refs/heads/") {
            if let Some(short) = cur_short.take() {
                out.insert(short, branch.trim().to_string());
            }
        } else if line.is_empty() {
            cur_short = None;
        }
    }
    out
}

/// The on-disk worktree directory to actually materialize `branch` into,
/// disambiguated against every other branch already occupying a `w/<short>`
/// slot (RAL-100 follow-up): two branches that [`worktree_dir`] would collide
/// on (e.g. `test-pr-submission-a` and `test-pr-submission-b`, which agree on
/// every character [`crate::short_paths::short_name`] looks at) get distinct
/// directories, `<short>` and `<short>-2`, instead of silently sharing one
/// worktree and racing on its git index.
///
/// Queries live git state (via [`existing_task_worktree_branches`]) rather
/// than an in-memory set, and reuses the exact suffix scheme
/// [`crate::short_paths::dedupe_short_names`] uses for guardian branches
/// (`-2`, `-3`, ...) -- but computed incrementally here since, unlike a
/// guardian's fixed branch list, task-worktree branches are discovered one
/// `ensure_worktree` call at a time, potentially across many squads over the
/// project's lifetime. That live query is also what makes this safe under
/// concurrent squads: [`resolve_placeholders`] runs its whole pass under the
/// daemon's global store lock, so by the time a second squad's call re-reads
/// `git worktree list`, the first squad's `git worktree add` has already
/// landed and claims its slot.
///
/// A directory already occupied by `branch` itself is reused as-is (restart
/// safety: the branch keeps the same directory across a squad restart). A
/// directory occupied by any other branch is skipped in favor of the next
/// suffix. A short name with no worktree registered at all is free.
fn resolve_task_worktree_dir(root: &Path, branch: &str) -> PathBuf {
    let existing = existing_task_worktree_branches(root);
    let base = crate::short_paths::short_name(branch);
    let mut candidate = base.clone();
    let mut n = 2;
    loop {
        match existing.get(&candidate) {
            None => break,
            Some(owner) if owner == branch => break,
            Some(_) => {
                candidate = format!("{base}-{n}");
                n += 1;
            }
        }
    }
    crate::short_paths::ralphus_root(root)
        .join("w")
        .join(candidate)
}

/// The OS path-length budget a worktree checkout is held to. Windows'
/// `MAX_PATH` is 260 characters; other platforms' limits are high enough in
/// practice that enforcing it there too would only produce false failures.
fn path_budget_limit() -> usize {
    if cfg!(windows) { 260 } else { usize::MAX }
}

/// Preflight (RAL-211): before checking anything out, measure whether the
/// worktree directory plus the deepest tracked path in `git_ref` would
/// overflow `limit`, and fail with the arithmetic spelled out rather than
/// letting git fail deep inside `worktree add` with an opaque `Filename too
/// long`. Callers pass [`path_budget_limit`]; taken as a plain parameter here
/// (rather than read directly) so this is testable without depending on the
/// host OS.
///
/// Reads the tree from the object database (`git ls-tree`, not `ls-files`,
/// which would describe the current checkout rather than the tree about to be
/// materialized) -- no checkout, no network.
fn preflight_worktree_budget(
    root: &Path,
    wt: &Path,
    git_ref: &str,
    limit: usize,
) -> Result<(), String> {
    if limit == usize::MAX {
        return Ok(());
    }
    let listing = git(root, &["ls-tree", "-r", "-z", "--name-only", git_ref]).map_err(|e| {
        format!("could not measure \"{git_ref}\" for a worktree path preflight check: {e}")
    })?;
    crate::short_paths::check_worktree_path_budget(wt, &listing, limit)
}

fn validate_branch_name(root: &Path, branch: &str) -> Result<(), String> {
    git(root, &["check-ref-format", "--branch", branch]).map(|_| ())
}

/// Resolve a placeholder cell `cwd`'s `?upstream=` value against the project
/// `root` (RAL-258). The two reserved `<<...>>` sentinels are expanded to a
/// concrete branch by querying live git state in `root`; any literal branch
/// name or `<remote>/<branch>` value is passed through unchanged.
///
/// `<<default>>` becomes the repository's default branch (the branch the
/// `origin` remote's HEAD symref points at); `<<current_branch>>` becomes
/// whatever branch `root` currently has checked out. Lookup failures are hard
/// errors — ralphus never guesses `main`/`master`, consistent with RAL-100's
/// no-guessing intent for `?upstream=`. Any other `<<...>>` value (a typo, or
/// an unsupported sentinel) is also rejected here, defense-in-depth on top of
/// the submit-time check in `ralphus_core::validate`.
fn resolve_upstream(root: &Path, upstream: &str) -> Result<String, String> {
    use ralphus_core::schema::{WORKTREE_UPSTREAM_CURRENT_BRANCH, WORKTREE_UPSTREAM_DEFAULT};
    match upstream {
        WORKTREE_UPSTREAM_DEFAULT => default_branch(root),
        WORKTREE_UPSTREAM_CURRENT_BRANCH => current_checked_out_branch(root),
        other if other.starts_with("<<") => Err(format!(
            "\"?upstream={other}\" is not a supported sentinel; use \"{WORKTREE_UPSTREAM_DEFAULT}\" \
             or \"{WORKTREE_UPSTREAM_CURRENT_BRANCH}\", or a literal branch name"
        )),
        other => Ok(other.to_string()),
    }
}

/// The repository's default branch for `root`: the branch the `origin`
/// remote's `HEAD` symbolic ref points at (the ref `git clone` sets on first
/// clone). Prefers the conventional `origin` remote, then falls back to any
/// other configured remote's `HEAD`, so a repo whose origin is named
/// differently still resolves rather than failing on a naming accident.
///
/// On lookup failure — no remote at all, or no remote `HEAD` symref (a repo
/// never cloned or fetched with its HEAD resolved) — this FAILS with a clear
/// error rather than guessing `main`/`master`, consistent with RAL-100's
/// no-guessing intent.
fn default_branch(root: &Path) -> Result<String, String> {
    let mut candidates = vec!["origin".to_string()];
    if let Ok(remotes) = git(root, &["remote"]) {
        let mut rest: Vec<String> = remotes
            .lines()
            .map(str::trim)
            .filter(|r| !r.is_empty() && *r != "origin")
            .map(str::to_string)
            .collect();
        rest.sort();
        candidates.extend(rest);
    }
    for remote in &candidates {
        let symref = format!("refs/remotes/{remote}/HEAD");
        let Ok(out) = git(root, &["symbolic-ref", &symref]) else {
            continue;
        };
        let refname = out.trim();
        // `refs/remotes/<remote>/<branch>` → the bare `<branch>`.
        let branch = refname
            .strip_prefix("refs/remotes/")
            .and_then(|r| r.split_once('/'))
            .map(|(_, b)| b)
            .unwrap_or(refname);
        if !branch.is_empty() {
            return Ok(branch.to_string());
        }
    }
    Err(format!(
        "could not resolve \"?upstream=<<default>>\" in {}: no remote HEAD symbolic ref \
         (refs/remotes/<remote>/HEAD) exists — set one (e.g. `git remote set-head origin \
         --auto`) or use a literal \"?upstream=<branch>\" value instead",
        root.display()
    ))
}

/// The branch currently checked out in the project's registered root/primary
/// worktree `root` — what `?upstream=<<current_branch>>` names. Resolves
/// against the *root* (the registered project path), NOT the not-yet-created
/// new worktree, which doesn't exist at resolution time. Uses the same
/// `git symbolic-ref --quiet --short HEAD` technique as
/// `crate::reviews::worktree_branch`.
fn current_checked_out_branch(root: &Path) -> Result<String, String> {
    let out = git(root, &["symbolic-ref", "--quiet", "--short", "HEAD"]).map_err(|_| {
        format!(
            "could not resolve \"?upstream=<<current_branch>>\" in {}: no branch checked out \
             (detached HEAD)",
            root.display()
        )
    })?;
    let branch = out.trim();
    if branch.is_empty() {
        return Err(format!(
            "could not resolve \"?upstream=<<current_branch>>\" in {}: no branch checked out",
            root.display()
        ));
    }
    Ok(branch.to_string())
}

fn branch_materialization(root: &Path, branch: &str) -> Result<BranchMaterialization, String> {
    validate_branch_name(root, branch)?;
    let branch_ref = format!("refs/heads/{branch}");
    if git(root, &["rev-parse", "--verify", &branch_ref]).is_ok() {
        return Ok(BranchMaterialization::ExistingLocal);
    }
    if branch.contains('/') {
        let remote_ref = format!("refs/remotes/{branch}");
        if git(root, &["rev-parse", "--verify", &remote_ref]).is_ok() {
            return Ok(BranchMaterialization::NewFromRemote { remote_ref });
        }
    }
    let warning = branch.contains('/').then(|| {
        format!(
            "placeholder branch \"{branch}\" looks like <remote>/<branch>, but \
             refs/remotes/{branch} does not exist in {}. Reusing or creating a \
             literal local branch named \"{branch}\" instead.",
            root.display()
        )
    });
    Ok(BranchMaterialization::NewFromHead { warning })
}

/// Set `branch`'s (checked out in worktree `wt`) upstream to the explicit
/// `?upstream=` value from a `ralphus:new-worktree/<branch>?upstream=<upstream>`
/// placeholder ([`ensure_worktree`]'s `upstream` parameter is now mandatory --
/// see [`ralphus_core::schema::parse_worktree_placeholder_upstream`]). Unlike
/// the old best-effort HEAD-inferred default this replaced, a failure here is
/// a hard error: the user named this upstream explicitly, so a value that
/// doesn't resolve (a typo, or a remote branch that hasn't been fetched into
/// `root` yet) must surface immediately rather than leaving the branch
/// silently untracked.
///
/// Deliberately does NOT delegate to `git branch --set-upstream-to <upstream>`
/// with the raw string: git resolves that through a DWIM ref search across
/// `refs/heads/`, `refs/remotes/`, etc., which reports `<upstream>` as
/// "ambiguous" whenever it matches refs in more than one namespace -- exactly
/// what happens for e.g. `?upstream=origin/foo` on a branch [`ensure_worktree`]
/// itself may have just created *literally named* `origin/foo` while tracking
/// remote-tracking ref `refs/remotes/origin/foo` (the `NewFromRemote` case
/// above). Resolving `upstream` ourselves first -- an unambiguous, fully
/// qualified `rev-parse --verify` against each candidate namespace -- and then
/// writing `branch.<branch>.remote`/`.merge` directly sidesteps that ambiguity
/// entirely. A remote-tracking match is preferred over a same-named local
/// branch, since tracking a remote is `?upstream=`'s primary purpose.
fn set_explicit_upstream(wt: &Path, branch: &str, upstream: &str) -> Result<(), String> {
    // RAL-258: the reserved `<<...>>` sentinels are never literal branch names.
    // They must be expanded by `resolve_upstream` before materialization; a
    // literal `<<...>>` here means resolution was skipped, not a real ref to
    // track (a branch named `<<default>>` would be meaningless and ambiguous).
    if upstream.starts_with("<<") {
        return Err(format!(
            "\"?upstream={upstream}\" is a reserved sentinel and must be resolved to a concrete \
             branch (via resolve_upstream) before materializing a worktree, not treated as a \
             literal branch name"
        ));
    }
    let fail = |e: String| -> String {
        format!(
            "could not set upstream to \"{upstream}\" for the worktree at {}: {e} -- the \
             \"?upstream=\" value must already exist as a local branch or a remote-tracking ref \
             (fetch the remote first if it names a \"<remote>/<branch>\" value)",
            wt.display()
        )
    };
    if let Some((remote, remote_branch)) = upstream.split_once('/') {
        let remote_ref = format!("refs/remotes/{upstream}");
        if git(wt, &["rev-parse", "--verify", &remote_ref]).is_ok() {
            git(wt, &["config", &format!("branch.{branch}.remote"), remote]).map_err(fail)?;
            git(
                wt,
                &[
                    "config",
                    &format!("branch.{branch}.merge"),
                    &format!("refs/heads/{remote_branch}"),
                ],
            )
            .map_err(fail)?;
            return Ok(());
        }
    }
    let local_ref = format!("refs/heads/{upstream}");
    if git(wt, &["rev-parse", "--verify", &local_ref]).is_ok() {
        git(wt, &["config", &format!("branch.{branch}.remote"), "."]).map_err(fail)?;
        git(
            wt,
            &["config", &format!("branch.{branch}.merge"), &local_ref],
        )
        .map_err(fail)?;
        return Ok(());
    }
    Err(fail("no matching ref found".to_string()))
}

/// If `wt`'s checked-out branch tracks a remote (its `@{upstream}` resolves
/// to a ref under `refs/remotes/...`), fetch that remote branch and rebase
/// `wt`'s local branch onto the freshly fetched commit — so a worktree
/// materialized from a `ralphus:new-worktree/<remote>/<branch>` placeholder
/// picks up new pushes to that remote branch on every resolution, rather than
/// freezing forever at whatever the remote-tracking ref held at first
/// materialization (the [`ensure_worktree`] reuse path never touched it
/// before this).
///
/// A no-op for a branch with no upstream, or whose upstream is a local branch
/// (e.g. the `NewFromHead` case's `--set-upstream-to <base>`) — only a
/// genuinely remote-tracked branch is resynced.
///
/// A rebase, deliberately not a hard reset: any commits already made in this
/// worktree (by a prior agent session, or by hand) are replayed on top of the
/// updated remote history rather than discarded. If the rebase can't
/// complete cleanly — a real conflict, or local history that has diverged
/// from the remote in an unresolvable way — it is aborted and the worktree is
/// left exactly as it was before this call; resolving that is left to a
/// human, not attempted here.
fn resync_remote_tracking_branch(wt: &Path) -> Result<(), String> {
    let Ok(upstream) = git(wt, &["rev-parse", "--symbolic-full-name", "@{upstream}"]) else {
        return Ok(());
    };
    let upstream = upstream.trim();
    let Some(rest) = upstream.strip_prefix("refs/remotes/") else {
        return Ok(());
    };
    let Some((remote, remote_branch)) = rest.split_once('/') else {
        return Ok(());
    };
    let refspec = format!("{remote_branch}:{upstream}");
    git(wt, &["fetch", remote, &refspec]).map_err(|e| {
        format!(
            "could not fetch \"{remote}\" branch \"{remote_branch}\" to resync {}: {e}",
            wt.display()
        )
    })?;
    if git(wt, &["rebase", upstream]).is_err() {
        let _ = git(wt, &["rebase", "--abort"]);
        return Err(format!(
            "could not rebase {} onto updated \"{upstream}\" -- it likely has local commits \
             that conflict with new commits on the remote; resolve manually in that worktree \
             (e.g. run `git rebase {upstream}` there and fix conflicts) and rerun",
            wt.display()
        ));
    }
    Ok(())
}

/// Create (or reuse) a git worktree for `branch` under `root`, returning its
/// path. `upstream` is the branch's `?upstream=` value from its
/// `ralphus:new-worktree/<branch>?upstream=<upstream>` placeholder --
/// REQUIRED (submit-time validation in `ralphus_core::validate` rejects a
/// placeholder cwd with no `?upstream=`, and [`resolve_placeholders`]
/// re-checks it defensively for hand-edited/stale data) rather than inferred,
/// so ralphus never has to guess what a freshly materialized branch is meant
/// to track.
///
/// The path is [`resolve_task_worktree_dir`], not the plain [`worktree_dir`]
/// -- so a branch whose short name collides with another branch already
/// occupying that `w/<short>` slot lands in `w/<short>-2` instead of silently
/// reusing the other branch's worktree.
///
/// Restart-safe: if the worktree directory's `.git` already exists, the
/// directory is assumed to be a previously materialized worktree and is
/// reused rather than recreated (or erroring because the branch or directory
/// already exists). Either way -- fresh or reused -- `upstream` is
/// (re-)applied via [`set_explicit_upstream`] and the branch is resynced if
/// it tracks a remote; see [`resync_remote_tracking_branch`].
///
/// Branch names are first validated through `git check-ref-format --branch`,
/// so the daemon inherits git's exact acceptance rules instead of trying to
/// mirror them (including rejecting path-traversal shapes like `../foo`
/// before any path join or filesystem write happens).
///
/// If no local branch by that literal name exists but a remote-tracking ref
/// `refs/remotes/<branch>` does, the new local branch is created from that
/// remote-tracking ref — a real, attached branch checkout (never a detached
/// HEAD), so ordinary git operations (commit, diff, `@{upstream}`) behave
/// normally inside it. Otherwise the branch name is treated literally —
/// slashes included — and a new local branch is forked from `root`'s current
/// `HEAD`; when that literal name *looks* like `<remote>/<branch>` but no
/// matching remote-tracking ref exists, a warning is emitted so the fallback
/// is explicit rather than silently guessed. Either way, `upstream` -- not
/// the branch's own name or `root`'s `HEAD` -- decides what tracking gets
/// configured; a review worktree tracking a remote keeps up with pushes to
/// that branch over the life of the project via the resync described above.
pub fn ensure_worktree(root: &Path, branch: &str, upstream: &str) -> Result<PathBuf, String> {
    let materialization = branch_materialization(root, branch)?;
    let wt = resolve_task_worktree_dir(root, branch);
    if wt.join(".git").exists() {
        set_explicit_upstream(&wt, branch, upstream)?;
        resync_remote_tracking_branch(&wt)?;
        return Ok(wt);
    }
    if let Some(parent) = wt.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("could not create worktree parent directory: {e}"))?;
    }
    let wt_str = wt.to_string_lossy().to_string();
    match materialization {
        BranchMaterialization::ExistingLocal => {
            preflight_worktree_budget(root, &wt, branch, path_budget_limit())?;
            git(root, &["worktree", "add", &wt_str, branch])?;
        }
        BranchMaterialization::NewFromRemote { remote_ref } => {
            preflight_worktree_budget(root, &wt, &remote_ref, path_budget_limit())?;
            git(
                root,
                &[
                    "worktree",
                    "add",
                    "--track",
                    "-b",
                    branch,
                    &wt_str,
                    &remote_ref,
                ],
            )?;
        }
        BranchMaterialization::NewFromHead { warning } => {
            if let Some(warning) = warning {
                crate::rlog!(WARNING, "ralphus [scheduler] {warning}");
            }
            preflight_worktree_budget(root, &wt, "HEAD", path_budget_limit())?;
            git(root, &["worktree", "add", "-b", branch, &wt_str])?;
        }
    }
    set_explicit_upstream(&wt, branch, upstream)?;
    resync_remote_tracking_branch(&wt)?;
    Ok(wt)
}

fn project_startup_adapter(vcs: &str) -> Option<&'static dyn ProjectStartupAdapter> {
    match vcs {
        "git" => Some(&GitProjectStartupAdapter),
        _ => None,
    }
}

fn placeholder_cache_key(machine: Option<&str>, placeholder: &str) -> String {
    format!("{}\u{0}{placeholder}", machine.unwrap_or(""))
}

fn placeholder_context_for_cell<'a>(
    squad_id: &'a str,
    cell: &'a CellRow,
) -> PlaceholderContext<'a> {
    PlaceholderContext {
        squad_id,
        task_idx: cell.task_idx,
        cell_idx: cell.idx,
        task_name: &cell.task_name,
        cell_id: &cell.cell_id,
        machine: cell.machine.as_deref(),
    }
}

fn synthetic_cell_row(ctx: PlaceholderContext<'_>) -> CellRow {
    CellRow {
        task_idx: ctx.task_idx,
        idx: ctx.cell_idx,
        task_name: ctx.task_name.to_string(),
        cell_id: ctx.cell_id.to_string(),
        cwd: None,
        subprojects: Vec::new(),
        prompt: None,
        command: None,
        agent: "raw".to_string(),
        model: None,
        system_prompt: None,
        system_prompt_position: None,
        depends_on: Vec::new(),
        timeout_sec: None,
        budget_tokens: None,
        maximum_budget_usd: None,
        upstream: None,
        machine: ctx.machine.map(str::to_string),
    }
}

impl ProjectStartupAdapter for GitProjectStartupAdapter {
    fn resolve_placeholder(
        &self,
        store: &Store,
        project: &ProjectView,
        placeholder: &str,
        ctx: PlaceholderContext<'_>,
    ) -> Result<Option<String>, String> {
        let Some(branch) = ralphus_core::schema::parse_worktree_placeholder(placeholder) else {
            return Ok(None);
        };
        let upstream = ralphus_core::schema::parse_worktree_placeholder_upstream(placeholder)
            .ok_or_else(|| {
                format!(
                    "cell '{}': placeholder \"{placeholder}\" is missing the required \
                         \"?upstream=<upstream>\" suffix (e.g. \"{}{branch}?upstream=main\")",
                    ctx.cell_id,
                    ralphus_core::schema::WORKTREE_PLACEHOLDER_PREFIX
                )
            })?;
        // RAL-258: expand any `?upstream=<<...>>` sentinel to a concrete branch
        // against the project root, so both local materialization and remote
        // provisioning act on a real tracking target -- never a literal
        // `<<...>>` (which no ref can resolve to, and which
        // `set_explicit_upstream` rejects). Failing to resolve is a hard error,
        // never a guess at `main`/`master`.
        let upstream = resolve_upstream(Path::new(&project.path), upstream)?;
        let resolved = match ctx.machine {
            Some(machine) if !machine.trim().is_empty() => provision_remote(
                store,
                machine,
                project,
                branch,
                &synthetic_cell_row(ctx),
                ctx.squad_id,
            )?,
            _ => ensure_worktree(Path::new(&project.path), branch, upstream)
                .map_err(|e| {
                    format!(
                        "cell '{}': could not materialize worktree for \"{placeholder}\": {e}",
                        ctx.cell_id
                    )
                })?
                .to_string_lossy()
                .into_owned(),
        };
        Ok(Some(resolved))
    }
}

fn expand_placeholder_text(
    raw: &str,
    mut resolve: impl FnMut(&str) -> Result<Option<String>, String>,
) -> Result<String, String> {
    let mut out = String::with_capacity(raw.len());
    let mut offset = 0usize;
    while let Some(open_rel) = raw[offset..].find("<<") {
        let open = offset + open_rel;
        out.push_str(&raw[offset..open]);
        let body_start = open + 2;
        let Some(close_rel) = raw[body_start..].find(">>") else {
            out.push_str(&raw[open..]);
            return Ok(out);
        };
        let close = body_start + close_rel;
        let body = &raw[body_start..close];
        if let Some(resolved) = resolve(body)? {
            out.push_str(&resolved);
        } else {
            out.push_str(&raw[open..close + 2]);
        }
        offset = close + 2;
    }
    out.push_str(&raw[offset..]);
    Ok(out)
}

fn validate_worktree_placeholders(raw: &str, cell_id: &str) -> Result<(), String> {
    let mut placeholders = Vec::new();
    if classify_placeholder(raw)?.is_some() {
        placeholders.push(raw);
    }
    placeholders.extend(
        ralphus_core::schema::text_placeholders(raw)
            .into_iter()
            .filter(|body| ralphus_core::schema::parse_worktree_placeholder(body).is_some()),
    );
    for placeholder in placeholders {
        let branch = ralphus_core::schema::parse_worktree_placeholder(placeholder)
            .expect("filtered to recognized placeholders");
        if ralphus_core::schema::parse_worktree_placeholder_upstream(placeholder).is_none() {
            return Err(format!(
                "cell '{cell_id}': placeholder \"{placeholder}\" is missing the required \
                 \"?upstream=<upstream>\" suffix (e.g. \"{}{branch}?upstream=main\")",
                ralphus_core::schema::WORKTREE_PLACEHOLDER_PREFIX
            ));
        }
    }
    Ok(())
}

fn resolve_placeholder_text_for_project(
    store: &Store,
    project_name: &str,
    raw: &str,
    ctx: PlaceholderContext<'_>,
    cache: &mut HashMap<String, String>,
) -> Result<String, String> {
    validate_worktree_placeholders(raw, ctx.cell_id)?;
    let project = store
        .resolve_project(project_name)
        .map_err(|e| e.to_string())?
        .ok_or_else(|| {
            format!(
                "cell '{}': project \"{project_name}\" is not registered",
                ctx.cell_id
            )
        })?;
    let Some(adapter) = project_startup_adapter(&project.vcs) else {
        return Ok(raw.to_string());
    };
    let mut resolve_known = |placeholder: &str| -> Result<Option<String>, String> {
        let key = placeholder_cache_key(ctx.machine, placeholder);
        if let Some(cached) = cache.get(&key) {
            return Ok(Some(cached.clone()));
        }
        let Some(resolved) = adapter.resolve_placeholder(store, &project, placeholder, ctx)? else {
            return Ok(None);
        };
        cache.insert(key, resolved.clone());
        Ok(Some(resolved))
    };
    if classify_placeholder(raw)?.is_some() {
        return resolve_known(raw)?.ok_or_else(|| raw.to_string());
    }
    expand_placeholder_text(raw, resolve_known)
}

pub(crate) fn materialize_env_overrides(
    store: &Store,
    ctx: PlaceholderContext<'_>,
    env: &BTreeMap<String, String>,
) -> Result<BTreeMap<String, String>, String> {
    let Some(project_name) = store
        .task_project_at(ctx.squad_id, ctx.task_idx)
        .map_err(|e| e.to_string())?
    else {
        return Ok(env.clone());
    };
    let mut cache = HashMap::new();
    env.iter()
        .map(|(key, value)| {
            resolve_placeholder_text_for_project(store, &project_name, value, ctx, &mut cache)
                .map(|resolved| (key.clone(), resolved))
        })
        .collect()
}

/// Classify a cell `cwd` as a placeholder needing materialization, a plain
/// path, or an unsupported scheme (RAL-185).
///
/// This is the registry-dispatch seam that generalizes the original hardcoded
/// `ralphus:new-worktree/` match. Today exactly one scheme materializes
/// anything — `ralphus:` — but a `cwd` carrying any *other* registered
/// provider's scheme is now recognized and rejected explicitly instead of
/// being silently treated as a literal directory name.
///
/// That silent-literal behavior is the failure this guards against: a `cwd` of
/// `incredibuild:/build/wt` would previously fall through
/// `parse_worktree_placeholder` as `None` and be handed to the runner verbatim,
/// which on Windows creates a *directory named `incredibuild:`* rather than
/// erroring — the agent would then run in the wrong place with no diagnostic.
///
/// Returns `Ok(Some(branch))` for a `ralphus:new-worktree/<branch>` placeholder,
/// `Ok(None)` for a plain filesystem path, and `Err` for a recognized-but-
/// unsupported scheme.
fn classify_placeholder(cwd: &str) -> Result<Option<&str>, String> {
    if let Some(branch) = ralphus_core::schema::parse_worktree_placeholder(cwd) {
        return Ok(Some(branch));
    }
    // A bare `ralphus:`-scheme value that isn't a well-formed new-worktree
    // placeholder is a typo, not a path -- `ralphus:` is this project's own
    // reserved scheme, so nothing legitimate starts with it.
    if let Some(rest) = cwd.strip_prefix("ralphus:") {
        return Err(format!(
            "\"ralphus:{rest}\" is not a valid placeholder; expected \
             \"{}<branch>\"",
            ralphus_core::schema::WORKTREE_PLACEHOLDER_PREFIX
        ));
    }
    // Any other `scheme:uri` that parses as a *machine* URI is a scheme in the
    // wrong field: the machine belongs in `machine = "..."`, and `cwd` stays a
    // path (or a `ralphus:new-worktree/` placeholder, which the cell's
    // machine then provisions remotely). Saying so beats silently creating a
    // directory named after the scheme.
    if let Ok(ralphus_core::schema::MachineRef::Provider { scheme, .. }) =
        ralphus_core::schema::parse_machine(cwd)
    {
        return Err(format!(
            "\"{scheme}:...\" is a machine, not a working directory — set it as \
             this cell's `machine` and give `cwd` a plain path or \
             \"{}<branch>\"",
            ralphus_core::schema::WORKTREE_PLACEHOLDER_PREFIX
        ));
    }
    Ok(None)
}

/// Ask a machine provider to provision a workspace for `branch`, returning
/// the path **on that machine** (RAL-185).
///
/// The `source` handed over is VCS-agnostic: `kind` comes from the registered
/// project's own `vcs` column, and `url` is only resolved for git. A provider
/// backing a non-git project reads `kind`, ignores the git-shaped fields, and
/// does whatever that source system needs — which is what keeps the daemon
/// from re-acquiring the hard git dependency RAL-175/184 baked in.
fn provision_remote(
    store: &Store,
    machine: &str,
    project: &crate::store::ProjectView,
    branch: &str,
    cell: &CellRow,
    squad_id: &str,
) -> Result<String, String> {
    let provider = crate::remote_runner::provider_from_store(store, machine)
        .map_err(|e| format!("cell '{}': {e}", cell.cell_id))?
        .ok_or_else(|| {
            // `provision_remote` is only called for a non-empty machine, so a
            // local resolution here means the value changed under us.
            format!(
                "cell '{}': machine \"{machine}\" resolved to the local host",
                cell.cell_id
            )
        })?;
    // Only git has a clone URL to resolve; every other kind gets `None` and
    // the provider decides how to obtain the source.
    let url = if project.vcs == "git" {
        crate::guardian_merge::remote_clone_url(Path::new(&project.path)).ok()
    } else {
        None
    };
    let req = crate::remote_runner::ProvisionRequest {
        project: project.name.clone(),
        source: crate::remote_runner::WorkspaceSource {
            kind: project.vcs.clone(),
            url,
            branch: Some(branch.to_string()),
        },
        squad_id: squad_id.to_string(),
        cell_id: cell.cell_id.clone(),
    };
    let spec = crate::runner::RunnerSpec::from_row(squad_id, cell);
    let result = provider
        .provision(&req, &spec)
        .map_err(|e| format!("cell '{}': {e}", cell.cell_id));
    // RAL-201: `provision` had no Cartographer coverage at all -- a failure
    // was only visible via the caller's generic "worktree placeholder
    // resolution failed" `rlog!` line, and success left no record whatsoever.
    crate::cartographer::Note::new("worktrees")
        .level(if result.is_ok() {
            crate::logging::LogLevel::INFO
        } else {
            crate::logging::LogLevel::WARNING
        })
        .scope("cell")
        .squad(squad_id)
        .cell(&cell.cell_id)
        .emit(
            store,
            "machine provision",
            serde_json::json!({
                "machine": machine,
                "branch": branch,
                "ok": result.is_ok(),
                "workspace": result.as_ref().ok(),
                "error": result.as_ref().err(),
            }),
        );
    result
}

/// Resolve every placeholder `cwd` (`ralphus:new-worktree/<branch>`) among
/// `cells` in place, persisting each resolution to the store. Each
/// cell's project comes from its owning task's `project` field in `tasks`,
/// not from the placeholder string itself. A placeholder string repeated
/// across cells/tasks is only materialized once per call (memoized in
/// `cache`), so concurrent cells sharing one worktree never race to create
/// it twice.
///
/// Wrapped in its own `scheduler.resolve_worktrees` span (RAL-96/RAL-100), a
/// child of `parent` (the owning squad's span), so a slow `git worktree add`
/// against a large repo is visible as its own timed step in the trace rather
/// than being folded into the surrounding `scheduler.run_execute` span.
///
/// Returns an error naming the first cell whose project can't be resolved
/// or whose worktree can't be created — submit-time validation should already
/// have ruled this out, but the scheduler must fail the squad cleanly rather
/// than panic on stale or hand-edited data (e.g. a project deregistered after
/// submit). The error is logged here (WARNING) before being returned, since
/// the scheduler's own failure path only records squad/task state transitions,
/// not the reason string itself.
pub fn resolve_placeholders(
    store: &Store,
    squad_id: &str,
    cells: &mut [CellRow],
    tasks: &[TaskRow],
    parent: &Context,
) -> Result<(), String> {
    let span = otel::start_span("scheduler.resolve_worktrees", parent, SpanKind::Internal);
    span.set_attribute("squad_id", squad_id.to_string());
    match resolve_placeholders_inner(store, squad_id, cells, tasks) {
        Ok(materialized) => {
            span.set_attribute("worktrees.materialized", materialized as i64);
            span.set_status(Status::Ok);
            Ok(())
        }
        Err(e) => {
            span.set_status(Status::error(e.clone()));
            crate::rlog!(
                WARNING,
                "ralphus [scheduler] squad {squad_id} worktree placeholder resolution failed: {e}"
            );
            Err(e)
        }
    }
}

/// The count returned is how many distinct placeholders were newly
/// materialized (i.e. not already resolved from a prior squad/restart or a
/// dedup hit within this call) — reported on the span as
/// `worktrees.materialized`.
fn resolve_placeholders_inner(
    store: &Store,
    squad_id: &str,
    cells: &mut [CellRow],
    tasks: &[TaskRow],
) -> Result<usize, String> {
    let task_projects: HashMap<i64, Option<&str>> = tasks
        .iter()
        .map(|t| (t.idx, t.project.as_deref()))
        .collect();
    let mut cache: HashMap<String, String> = HashMap::new();
    let mut materialized = 0usize;
    for cell in cells.iter_mut() {
        let Some(cwd) = cell.cwd.clone() else {
            continue;
        };
        classify_placeholder(&cwd).map_err(|e| {
            format!(
                "cell '{}': could not resolve cwd \"{cwd}\": {e}",
                cell.cell_id
            )
        })?;
        let Some(project_name) = task_projects.get(&cell.task_idx).copied().flatten() else {
            if ralphus_core::schema::first_worktree_placeholder_in_text(&cwd).is_some() {
                return Err(format!(
                    "cell '{}': task \"{}\" has no 'project' set for its placeholder cwd",
                    cell.cell_id, cell.task_name
                ));
            }
            continue;
        };
        let cache_before = cache.len();
        let resolved = resolve_placeholder_text_for_project(
            store,
            project_name,
            &cwd,
            placeholder_context_for_cell(squad_id, cell),
            &mut cache,
        )
        .map_err(|e| {
            format!(
                "cell '{}': could not resolve cwd \"{cwd}\": {e}",
                cell.cell_id
            )
        })?;
        materialized += cache.len().saturating_sub(cache_before);
        if resolved == cwd {
            continue;
        }
        store
            .set_cell_cwd(squad_id, cell.task_idx, cell.idx, &resolved)
            .map_err(|e| e.to_string())?;
        crate::rlog!(
            INFO,
            "ralphus [scheduler] cell {squad_id}/{} cwd placeholder \"{cwd}\" resolved to {resolved}",
            cell.cell_id
        );
        cell.cwd = Some(resolved);
    }
    Ok(materialized)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    static TEST_N: AtomicU32 = AtomicU32::new(0);

    fn tmp_dir(tag: &str) -> PathBuf {
        let n = TEST_N.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("ral100-wt-{tag}-{}-{n}", std::process::id()));
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
            .status()
            .expect("git");
        assert!(
            status.success(),
            "git {args:?} in {} failed",
            root.display()
        );
    }

    /// A fresh repo with one commit on `main`.
    fn init_repo(tag: &str) -> PathBuf {
        let repo = tmp_dir(tag);
        g(&repo, &["init", "-b", "main"]);
        std::fs::write(repo.join("base.txt"), "base\n").unwrap();
        g(&repo, &["add", "."]);
        g(&repo, &["commit", "-m", "base"]);
        repo
    }

    fn init_repo_with_remote_branch(tag: &str, branch: &str) -> (PathBuf, String) {
        let base = tmp_dir(tag);
        let remote = base.join("remote.git");
        let (_, remote_branch) = branch
            .split_once('/')
            .expect("remote-qualified branch placeholder");
        g(
            &base,
            &[
                "init",
                "--bare",
                "--initial-branch=main",
                remote.to_str().expect("remote path"),
            ],
        );

        let seed = base.join("seed");
        std::fs::create_dir_all(&seed).unwrap();
        g(&seed, &["init", "-b", "main"]);
        std::fs::write(seed.join("base.txt"), "base\n").unwrap();
        g(&seed, &["add", "."]);
        g(&seed, &["commit", "-m", "base"]);
        g(
            &seed,
            &[
                "remote",
                "add",
                "origin",
                remote.to_str().expect("remote path"),
            ],
        );
        g(&seed, &["push", "-u", "origin", "main"]);

        g(&seed, &["checkout", "-b", remote_branch]);
        std::fs::write(seed.join("remote-only.txt"), format!("{branch}\n")).unwrap();
        g(&seed, &["add", "."]);
        g(&seed, &["commit", "-m", "remote branch"]);
        let branch_sha = git(&seed, &["rev-parse", "HEAD"])
            .expect("branch sha")
            .trim()
            .to_string();
        g(&seed, &["push", "-u", "origin", remote_branch]);

        let clone = base.join("clone");
        g(
            &base,
            &[
                "clone",
                remote.to_str().expect("remote path"),
                clone.to_str().expect("clone path"),
            ],
        );
        (clone, branch_sha)
    }

    fn cell_row(task_idx: i64, idx: i64, cell_id: &str, cwd: Option<&str>) -> CellRow {
        CellRow {
            task_idx,
            idx,
            task_name: format!("task{task_idx}"),
            cell_id: cell_id.to_string(),
            cwd: cwd.map(str::to_string),
            subprojects: vec![],
            prompt: Some("do work".to_string()),
            command: None,
            agent: "claude".to_string(),
            model: None,
            system_prompt: None,
            system_prompt_position: None,
            depends_on: vec![],
            timeout_sec: None,
            budget_tokens: None,
            maximum_budget_usd: None,
            upstream: None,
            machine: None,
        }
    }

    fn task_row(idx: i64, project: Option<&str>) -> TaskRow {
        TaskRow {
            idx,
            name: format!("task{idx}"),
            project: project.map(str::to_string),
            depends_on: vec![],
            soloed: false,
        }
    }

    #[test]
    fn ensure_worktree_creates_new_branch_and_worktree() {
        let repo = init_repo("new-branch");
        let wt = ensure_worktree(&repo, "feature-x", "main").expect("materialize");
        assert_eq!(wt, worktree_dir(&repo, "feature-x"));
        assert!(
            wt.join(".git").exists(),
            "worktree must be a real linked worktree"
        );
        assert_eq!(
            git(&wt, &["symbolic-ref", "--short", "HEAD"])
                .unwrap()
                .trim(),
            "feature-x"
        );
    }

    #[test]
    fn ensure_worktree_disambiguates_branches_sharing_a_short_name() {
        // "test-pr-submission-a" and "test-pr-submission-b" agree on every
        // character short_name's 12-character truncation window looks at, so
        // both compute the plain `worktree_dir` short name "test-pr". Without
        // disambiguation the second `ensure_worktree` call would silently
        // reuse the first branch's worktree instead of creating its own.
        let repo = init_repo("collide");
        let wt_a = ensure_worktree(&repo, "test-pr-submission-a", "main").expect("materialize a");
        let wt_b = ensure_worktree(&repo, "test-pr-submission-b", "main").expect("materialize b");

        assert_ne!(
            wt_a, wt_b,
            "colliding branches must land in distinct worktrees"
        );
        assert_eq!(wt_a, worktree_dir(&repo, "test-pr-submission-a"));

        assert_eq!(
            git(&wt_a, &["symbolic-ref", "--short", "HEAD"])
                .unwrap()
                .trim(),
            "test-pr-submission-a"
        );
        assert_eq!(
            git(&wt_b, &["symbolic-ref", "--short", "HEAD"])
                .unwrap()
                .trim(),
            "test-pr-submission-b"
        );

        // Calling again for the same branches must reuse the same two
        // directories rather than drifting to a third (restart safety).
        assert_eq!(
            wt_a,
            ensure_worktree(&repo, "test-pr-submission-a", "main").unwrap()
        );
        assert_eq!(
            wt_b,
            ensure_worktree(&repo, "test-pr-submission-b", "main").unwrap()
        );
    }

    #[test]
    fn ensure_worktree_sets_the_explicit_upstream_it_is_given() {
        // reviews.rs::derive_reviews requires `@{upstream}` to be resolvable on
        // every review-opted-in cell's branch to determine the review base.
        // A brand-new `ralphus:new-worktree/...` branch must come out of
        // `ensure_worktree` with the caller's explicit `upstream` argument
        // already configured, or every such placeholder+review combination
        // would fail preflight.
        let repo = init_repo("new-branch-upstream");
        let wt = ensure_worktree(&repo, "feature-z", "main").expect("materialize");
        let upstream = git(
            &wt,
            &[
                "rev-parse",
                "--abbrev-ref",
                "--symbolic-full-name",
                "@{upstream}",
            ],
        )
        .expect("new branch must have an upstream configured");
        assert_eq!(upstream.trim(), "main");
    }

    #[test]
    fn ensure_worktree_fails_when_the_explicit_upstream_does_not_exist() {
        // Unlike the old best-effort HEAD-inferred default, an explicit
        // `upstream` that doesn't resolve to any branch is a hard error, not
        // a silently-untracked branch.
        let repo = init_repo("bad-explicit-upstream");
        let err = ensure_worktree(&repo, "feature-w", "does-not-exist")
            .expect_err("a nonexistent upstream must fail");
        assert!(err.contains("does-not-exist"), "{err}");
    }

    #[test]
    fn resolve_upstream_expands_the_default_sentinel_via_origin_head() {
        // `init_repo_with_remote_branch` clones a remote seeded with a `main`
        // default branch, so the clone's `refs/remotes/origin/HEAD` resolves
        // to `main` — `<<default>>` must expand to it.
        let (repo, _) = init_repo_with_remote_branch("sentinel-default", "origin/foo");
        assert_eq!(
            resolve_upstream(&repo, "<<default>>").expect("default sentinel"),
            "main"
        );
        // And a literal value is passed through untouched.
        assert_eq!(resolve_upstream(&repo, "main").unwrap(), "main");
        assert_eq!(resolve_upstream(&repo, "origin/foo").unwrap(), "origin/foo");
    }

    #[test]
    fn resolve_upstream_expands_the_current_branch_sentinel_from_root() {
        let repo = init_repo("sentinel-current");
        assert_eq!(
            resolve_upstream(&repo, "<<current_branch>>").expect("current branch sentinel"),
            "main"
        );
        // Following the project's checked-out branch moves the sentinel with it
        // (the riskier, run-varying behavior the tutor flags).
        g(&repo, &["checkout", "-b", "some-feature"]);
        assert_eq!(
            resolve_upstream(&repo, "<<current_branch>>").unwrap(),
            "some-feature"
        );
    }

    #[test]
    fn resolve_upstream_default_fails_without_a_remote_head_and_never_guesses() {
        // `init_repo` has no remote at all, so `refs/remotes/origin/HEAD`
        // cannot resolve. `<<default>>` must FAIL with a clear error rather
        // than guessing `main`/`master`.
        let repo = init_repo("sentinel-no-remote");
        let err = resolve_upstream(&repo, "<<default>>")
            .expect_err("no remote HEAD must be a hard error, not a guess");
        assert!(
            err.contains("<<default>>") && err.contains("no remote HEAD"),
            "{err}"
        );
    }

    #[test]
    fn resolve_upstream_rejects_an_unknown_sentinel_defensively() {
        let repo = init_repo("sentinel-unknown");
        let err = resolve_upstream(&repo, "<<wat>>").expect_err("unknown sentinel must fail");
        assert!(
            err.contains("<<wat>>") && err.contains("<<current_branch>>"),
            "{err}"
        );
    }

    #[test]
    fn preflight_worktree_budget_fails_fast_with_the_arithmetic_spelled_out() {
        // The limit is passed explicitly rather than read from the host OS
        // (see `path_budget_limit`), so this is deterministic on every
        // platform CI runs on.
        let repo = init_repo("preflight-tight");
        let err = preflight_worktree_budget(&repo, Path::new("/short/wt"), "HEAD", 5)
            .expect_err("must fail when the budget is obviously too tight");
        assert!(err.contains("5-character"), "{err}");
    }

    #[test]
    fn preflight_worktree_budget_passes_with_a_generous_limit() {
        let repo = init_repo("preflight-loose");
        preflight_worktree_budget(&repo, Path::new("/short/wt"), "HEAD", 4096)
            .expect("must pass with a generous budget");
    }

    #[test]
    fn preflight_worktree_budget_is_a_noop_when_the_limit_is_max() {
        // The non-Windows branch of `path_budget_limit` -- never measures,
        // never fails, regardless of how deep the tree is.
        let repo = init_repo("preflight-unlimited");
        preflight_worktree_budget(&repo, Path::new("/short/wt"), "HEAD", usize::MAX)
            .expect("usize::MAX must always pass");
    }

    #[test]
    fn ensure_worktree_attaches_to_pre_existing_branch() {
        let repo = init_repo("existing-branch");
        g(&repo, &["branch", "already-here"]);
        let wt = ensure_worktree(&repo, "already-here", "main").expect("materialize");
        assert_eq!(
            git(&wt, &["symbolic-ref", "--short", "HEAD"])
                .unwrap()
                .trim(),
            "already-here"
        );
    }

    #[test]
    fn ensure_worktree_uses_the_new_short_layout_and_ignores_old_layout_leftovers() {
        // An old-layout `.ralphus_worktrees/<branch>` directory left over
        // from before RAL-211 must not confuse or block a fresh
        // materialization under the `.ralphus/w/<short>` layout -- old-layout
        // state is simply left alone.
        let repo = init_repo("old-layout-coexist");
        let old_dir = repo
            .join(".git")
            .join(".ralphus_worktrees")
            .join("feature-long-name");
        std::fs::create_dir_all(&old_dir).unwrap();
        std::fs::write(old_dir.join("leftover.txt"), "abandoned\n").unwrap();

        let wt = ensure_worktree(&repo, "feature-long-name", "main").expect("materialize");
        assert_eq!(wt, worktree_dir(&repo, "feature-long-name"));
        assert!(
            wt.to_string_lossy().contains(".ralphus")
                && !wt.to_string_lossy().contains(".ralphus_worktrees"),
            "must use the new layout, not the old one: {}",
            wt.display()
        );
        assert!(wt.join(".git").exists());
        // The old leftover must still be untouched -- no migration/purge.
        assert!(old_dir.join("leftover.txt").exists());
    }

    #[test]
    fn ensure_worktree_bootstraps_a_remote_tracking_branch_when_it_exists() {
        let (repo, remote_sha) = init_repo_with_remote_branch("remote-branch", "origin/foo");
        let wt =
            ensure_worktree(&repo, "origin/foo", "origin/foo").expect("materialize from remote");
        assert_eq!(wt, worktree_dir(&repo, "origin/foo"));
        assert!(
            wt.join("remote-only.txt").exists(),
            "the remote branch's content must be present in the worktree"
        );
        assert_eq!(
            git(&wt, &["symbolic-ref", "--short", "HEAD"])
                .unwrap()
                .trim(),
            "origin/foo"
        );
        assert_eq!(git(&wt, &["rev-parse", "HEAD"]).unwrap().trim(), remote_sha);
        assert_eq!(
            git(
                &wt,
                &[
                    "rev-parse",
                    "--abbrev-ref",
                    "--symbolic-full-name",
                    "@{upstream}",
                ],
            )
            .unwrap()
            .trim(),
            "remotes/origin/foo"
        );
    }

    #[test]
    fn ensure_worktree_resyncs_a_remote_tracking_branch_to_new_pushes() {
        let (repo, first_sha) = init_repo_with_remote_branch("resync-new-push", "origin/foo");
        let wt = ensure_worktree(&repo, "origin/foo", "origin/foo").expect("first materialize");
        assert_eq!(git(&wt, &["rev-parse", "HEAD"]).unwrap().trim(), first_sha);

        // Simulate a new push to the remote branch, from the seed clone that
        // pushed the original commit.
        let seed = repo.parent().unwrap().join("seed");
        std::fs::write(seed.join("remote-only.txt"), "origin/foo v2\n").unwrap();
        g(&seed, &["commit", "-am", "second remote commit"]);
        let second_sha = git(&seed, &["rev-parse", "HEAD"])
            .unwrap()
            .trim()
            .to_string();
        g(&seed, &["push", "origin", "foo"]);

        // Re-resolving the same (already materialized) worktree must pick up
        // the new remote commit, not stay frozen at the first-materialize SHA.
        let wt2 = ensure_worktree(&repo, "origin/foo", "origin/foo").expect("resync on reuse");
        assert_eq!(wt2, wt);
        assert_eq!(git(&wt, &["rev-parse", "HEAD"]).unwrap().trim(), second_sha);
    }

    #[test]
    fn ensure_worktree_rebases_local_commits_onto_the_resynced_remote_instead_of_discarding_them() {
        let (repo, _first_sha) = init_repo_with_remote_branch("resync-rebase-local", "origin/bar");
        let wt = ensure_worktree(&repo, "origin/bar", "origin/bar").expect("first materialize");

        // A prior agent session committed local work directly in the worktree.
        std::fs::write(wt.join("local-work.txt"), "agent work\n").unwrap();
        g(&wt, &["add", "."]);
        g(&wt, &["commit", "-m", "local agent commit"]);

        // A new, unrelated commit lands on the remote branch.
        let seed = repo.parent().unwrap().join("seed");
        std::fs::write(seed.join("remote-only.txt"), "origin/bar v2\n").unwrap();
        g(&seed, &["commit", "-am", "second remote commit"]);
        g(&seed, &["push", "origin", "bar"]);

        ensure_worktree(&repo, "origin/bar", "origin/bar").expect("resync via rebase");

        assert!(
            wt.join("local-work.txt").exists(),
            "the local commit must survive the resync, replayed on top of the new remote commit \
             rather than discarded by a hard reset"
        );
        assert_eq!(
            git(&wt, &["log", "--format=%s", "-1"]).unwrap().trim(),
            "local agent commit",
            "the local commit must remain the tip after being rebased forward"
        );
    }

    #[test]
    fn ensure_worktree_aborts_and_errors_when_the_resync_rebase_conflicts() {
        let (repo, _first_sha) = init_repo_with_remote_branch("resync-conflict", "origin/baz");
        let wt = ensure_worktree(&repo, "origin/baz", "origin/baz").expect("first materialize");

        // A local commit that edits the same file the remote is about to change.
        std::fs::write(wt.join("remote-only.txt"), "local edit\n").unwrap();
        g(&wt, &["commit", "-am", "local conflicting edit"]);

        let seed = repo.parent().unwrap().join("seed");
        std::fs::write(seed.join("remote-only.txt"), "remote edit\n").unwrap();
        g(&seed, &["commit", "-am", "conflicting remote edit"]);
        g(&seed, &["push", "origin", "baz"]);

        let err = ensure_worktree(&repo, "origin/baz", "origin/baz")
            .expect_err("conflicting rebase must fail");
        assert!(err.contains("rebase"), "{err}");

        // A failed resync must leave the worktree attached to its branch, not
        // mid-rebase (detached) -- i.e. the abort actually ran.
        assert_eq!(
            git(&wt, &["symbolic-ref", "--short", "HEAD"])
                .expect("worktree must not be left mid-rebase")
                .trim(),
            "origin/baz"
        );
    }

    #[test]
    fn ensure_worktree_warns_and_falls_back_to_a_literal_local_slash_branch() {
        let repo = init_repo("literal-slash-branch");
        let plan = branch_materialization(&repo, "alternative/foo").expect("plan");
        assert_eq!(
            plan,
            BranchMaterialization::NewFromHead {
                warning: Some(format!(
                    "placeholder branch \"alternative/foo\" looks like <remote>/<branch>, but \
             refs/remotes/alternative/foo does not exist in {}. Reusing or creating a \
             literal local branch named \"alternative/foo\" instead.",
                    repo.display()
                ),),
            }
        );

        let wt = ensure_worktree(&repo, "alternative/foo", "main").expect("materialize");
        assert_eq!(wt, worktree_dir(&repo, "alternative/foo"));
        assert_eq!(
            git(&wt, &["symbolic-ref", "--short", "HEAD"])
                .unwrap()
                .trim(),
            "alternative/foo"
        );
    }

    #[test]
    fn ensure_worktree_rejects_invalid_branch_names_before_path_joining() {
        let repo = init_repo("invalid-branch");
        let err =
            ensure_worktree(&repo, "../escape", "main").expect_err("invalid branch must fail");
        assert!(err.contains("check-ref-format"), "{err}");
        assert!(
            !repo.join(".git").join(".ralphus").exists(),
            "no worktree directory tree may be created for an invalid branch name"
        );
    }

    #[test]
    fn ensure_worktree_reuses_existing_worktree_without_wiping_it() {
        let repo = init_repo("reuse");
        let wt = ensure_worktree(&repo, "feature-y", "main").expect("first materialize");
        std::fs::write(wt.join("in-progress.txt"), "agent work\n").unwrap();

        // A second call (simulating a restarted squad) must reuse the same
        // worktree rather than recreating it, so the agent's uncommitted work
        // survives.
        let wt2 =
            ensure_worktree(&repo, "feature-y", "main").expect("second materialize (restart)");
        assert_eq!(wt, wt2);
        assert!(
            wt2.join("in-progress.txt").exists(),
            "restart must not wipe uncommitted work in an already-materialized worktree"
        );
    }

    #[test]
    fn classify_placeholder_recognizes_a_worktree_placeholder() {
        assert_eq!(
            classify_placeholder("ralphus:new-worktree/feat-a"),
            Ok(Some("feat-a"))
        );
    }

    #[test]
    fn classify_placeholder_passes_plain_paths_through() {
        assert_eq!(classify_placeholder("/home/me/repo"), Ok(None));
        assert_eq!(classify_placeholder(r"C:\repo\wt"), Ok(None));
        assert_eq!(classify_placeholder("."), Ok(None));
    }

    #[test]
    fn classify_placeholder_rejects_a_malformed_ralphus_scheme() {
        let err = classify_placeholder("ralphus:new-worktre/typo")
            .expect_err("a ralphus: typo must not be treated as a directory name");
        assert!(err.contains("not a valid placeholder"), "{err}");
    }

    #[test]
    fn classify_placeholder_rejects_a_provider_scoped_cwd_instead_of_making_a_directory() {
        // Regression guard (RAL-185): before scheme dispatch this fell through
        // as a plain path and Windows happily created a directory literally
        // named `incredibuild:`, so the agent ran in the wrong place with no
        // diagnostic at all.
        let err = classify_placeholder("incredibuild:/build/wt")
            .expect_err("a machine-shaped cwd must be rejected, not silently used as a path");
        assert!(
            err.contains("is a machine, not a working directory"),
            "{err}"
        );
        assert!(err.contains("incredibuild"), "{err}");
    }

    #[test]
    fn resolve_placeholders_surfaces_an_unsupported_scheme_as_a_squad_failure() {
        let store = Store::open_in_memory().unwrap();
        let mut cells = vec![cell_row(0, 0, "s0", Some("incredibuild:/build/wt"))];
        let err = resolve_placeholders(&store, "squad-1", &mut cells, &[], &Context::new())
            .expect_err("unsupported cwd scheme must fail the squad");
        assert!(err.contains("s0"), "error should name the cell: {err}");
        assert!(
            err.contains("is a machine, not a working directory"),
            "{err}"
        );
    }

    /// A provider script that echoes a fixed `provision` response.
    fn fake_provisioner(tag: &str, json: &str) -> PathBuf {
        let dir = tmp_dir(tag);
        let (path, body) = if cfg!(windows) {
            (dir.join("p.cmd"), format!("@echo off\r\necho {json}\r\n"))
        } else {
            (dir.join("p.sh"), format!("#!/bin/sh\necho '{json}'\n"))
        };
        std::fs::write(&path, body).expect("write provisioner");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        path
    }

    fn cell_row_on(
        task_idx: i64,
        idx: i64,
        cell_id: &str,
        cwd: Option<&str>,
        machine: Option<&str>,
    ) -> CellRow {
        let mut row = cell_row(task_idx, idx, cell_id, cwd);
        row.machine = machine.map(str::to_string);
        row
    }

    #[test]
    fn a_remote_cell_provisions_through_its_provider_instead_of_locally() {
        let repo = init_repo("remote-provision");
        let script = fake_provisioner(
            "remote-provision-p",
            r#"{"ok":true,"protocol_version":1,"workspace":"/remote/wt/feat-r"}"#,
        );
        let store = Store::open_in_memory().unwrap();
        store
            .register_project("proj", "", &repo.to_string_lossy(), "git")
            .unwrap();
        store
            .register_machine_provider(
                "ib",
                "",
                &script.to_string_lossy(),
                &[],
                crate::machines::PROTOCOL_VERSION,
                false,
            )
            .unwrap();
        let mut cells = vec![cell_row_on(
            0,
            0,
            "s0",
            Some("ralphus:new-worktree/feat-r?upstream=main"),
            Some("ib:A"),
        )];
        let tasks = vec![task_row(0, Some("proj"))];
        resolve_placeholders(&store, "squad-1", &mut cells, &tasks, &Context::new())
            .expect("remote provision");
        assert_eq!(cells[0].cwd.as_deref(), Some("/remote/wt/feat-r"));
        // Nothing may be created on the daemon's own disk for a remote cell.
        assert!(
            !worktree_dir(&repo, "feat-r").exists(),
            "a remote cell must not materialize a local worktree"
        );
    }

    #[test]
    fn the_same_placeholder_on_two_machines_resolves_to_two_workspaces() {
        // A cwd-only memo key would hand the second machine the first's path.
        let repo = init_repo("two-machines");
        let a = fake_provisioner(
            "two-machines-a",
            r#"{"ok":true,"protocol_version":1,"workspace":"/on/a"}"#,
        );
        let b = fake_provisioner(
            "two-machines-b",
            r#"{"ok":true,"protocol_version":1,"workspace":"/on/b"}"#,
        );
        let store = Store::open_in_memory().unwrap();
        store
            .register_project("proj", "", &repo.to_string_lossy(), "git")
            .unwrap();
        for (scheme, script) in [("ma", &a), ("mb", &b)] {
            store
                .register_machine_provider(
                    scheme,
                    "",
                    &script.to_string_lossy(),
                    &[],
                    crate::machines::PROTOCOL_VERSION,
                    false,
                )
                .unwrap();
        }
        let mut cells = vec![
            cell_row_on(
                0,
                0,
                "s0",
                Some("ralphus:new-worktree/shared?upstream=main"),
                Some("ma:1"),
            ),
            cell_row_on(
                1,
                0,
                "s1",
                Some("ralphus:new-worktree/shared?upstream=main"),
                Some("mb:1"),
            ),
        ];
        let tasks = vec![task_row(0, Some("proj")), task_row(1, Some("proj"))];
        resolve_placeholders(&store, "squad-1", &mut cells, &tasks, &Context::new())
            .expect("provision both");
        assert_eq!(cells[0].cwd.as_deref(), Some("/on/a"));
        assert_eq!(cells[1].cwd.as_deref(), Some("/on/b"));
    }

    #[test]
    fn a_provider_that_returns_no_workspace_path_fails_the_squad() {
        let repo = init_repo("no-workspace");
        let script = fake_provisioner("no-workspace-p", r#"{"ok":true,"protocol_version":1}"#);
        let store = Store::open_in_memory().unwrap();
        store
            .register_project("proj", "", &repo.to_string_lossy(), "git")
            .unwrap();
        store
            .register_machine_provider(
                "ib",
                "",
                &script.to_string_lossy(),
                &[],
                crate::machines::PROTOCOL_VERSION,
                false,
            )
            .unwrap();
        let mut cells = vec![cell_row_on(
            0,
            0,
            "s0",
            Some("ralphus:new-worktree/x?upstream=main"),
            Some("ib:A"),
        )];
        let tasks = vec![task_row(0, Some("proj"))];
        let err = resolve_placeholders(&store, "squad-1", &mut cells, &tasks, &Context::new())
            .expect_err("a provider that provisions nothing must fail the squad");
        assert!(err.contains("workspace"), "{err}");
    }

    #[test]
    fn resolve_placeholders_ignores_plain_cwd() {
        let repo = init_repo("plain-cwd");
        let store = Store::open_in_memory().unwrap();
        let plain = repo.to_string_lossy().into_owned();
        let mut cells = vec![cell_row(0, 0, "s0", Some(&plain))];
        resolve_placeholders(&store, "squad-1", &mut cells, &[], &Context::new())
            .expect("no-op for plain cwd");
        assert_eq!(cells[0].cwd.as_deref(), Some(plain.as_str()));
    }

    #[test]
    fn resolve_placeholders_fails_for_unregistered_project() {
        let store = Store::open_in_memory().unwrap();
        let mut cells = vec![cell_row(
            0,
            0,
            "s0",
            Some("ralphus:new-worktree/feat?upstream=main"),
        )];
        let tasks = vec![task_row(0, Some("ghost-project"))];
        let err = resolve_placeholders(&store, "squad-1", &mut cells, &tasks, &Context::new())
            .expect_err("unregistered project must fail");
        assert!(
            err.contains("ghost-project"),
            "error should name the project: {err}"
        );
    }

    #[test]
    fn resolve_placeholders_fails_when_task_has_no_project() {
        // Submit-time validation should already rule this out, but the
        // scheduler must still fail cleanly on stale/hand-edited data.
        let store = Store::open_in_memory().unwrap();
        let mut cells = vec![cell_row(
            0,
            0,
            "s0",
            Some("ralphus:new-worktree/feat?upstream=main"),
        )];
        let tasks = vec![task_row(0, None)];
        let err = resolve_placeholders(&store, "squad-1", &mut cells, &tasks, &Context::new())
            .expect_err("missing task project must fail");
        assert!(
            err.contains("no 'project' set"),
            "error should explain the missing project: {err}"
        );
    }

    #[test]
    fn resolve_placeholders_fails_when_upstream_is_missing_from_a_placeholder_cwd() {
        // Submit-time validation (`ralphus_core::validate`) already requires
        // every placeholder cwd to carry an explicit `?upstream=`; this is the
        // scheduler's own defensive re-check for stale/hand-edited data.
        let store = Store::open_in_memory().unwrap();
        let mut cells = vec![cell_row(0, 0, "s0", Some("ralphus:new-worktree/feat"))];
        let tasks = vec![task_row(0, Some("proj"))];
        let err = resolve_placeholders(&store, "squad-1", &mut cells, &tasks, &Context::new())
            .expect_err("a placeholder cwd with no ?upstream= must fail");
        assert!(err.contains("upstream"), "{err}");
        assert!(err.contains("s0"), "error should name the cell: {err}");
    }

    #[test]
    fn resolve_placeholders_honors_an_explicit_upstream_query_override() {
        // The `?upstream=` value, not the branch's own name or the repo's
        // HEAD, decides what the freshly materialized branch tracks.
        let repo = init_repo("explicit-upstream");
        g(&repo, &["checkout", "-b", "other"]);
        std::fs::write(repo.join("other.txt"), "on other\n").unwrap();
        g(&repo, &["add", "."]);
        g(&repo, &["commit", "-m", "other branch commit"]);
        g(&repo, &["checkout", "main"]);

        let store = Store::open_in_memory().unwrap();
        store
            .register_project("proj", "", &repo.to_string_lossy(), "git")
            .unwrap();
        let mut cells = vec![cell_row(
            0,
            0,
            "s0",
            Some("ralphus:new-worktree/feat-override?upstream=other"),
        )];
        let tasks = vec![task_row(0, Some("proj"))];
        resolve_placeholders(&store, "squad-1", &mut cells, &tasks, &Context::new())
            .expect("materialize");
        let resolved = cells[0].cwd.clone().expect("resolved cwd");
        assert_eq!(
            git(
                Path::new(&resolved),
                &[
                    "rev-parse",
                    "--abbrev-ref",
                    "--symbolic-full-name",
                    "@{upstream}",
                ],
            )
            .unwrap()
            .trim(),
            "other",
            "the branch must track the explicit ?upstream= value, not HEAD (main)"
        );
    }

    #[test]
    fn resolve_placeholders_materializes_a_registered_placeholder() {
        let repo = init_repo("materialize");
        let store = Store::open_in_memory().unwrap();
        store
            .register_project("myproj", "", &repo.to_string_lossy(), "git")
            .unwrap();
        let mut cells = vec![cell_row(
            0,
            0,
            "s0",
            Some("ralphus:new-worktree/feat-a?upstream=main"),
        )];
        let tasks = vec![task_row(0, Some("myproj"))];
        resolve_placeholders(&store, "squad-1", &mut cells, &tasks, &Context::new())
            .expect("materialize");
        let resolved = cells[0].cwd.clone().expect("resolved cwd");
        assert_eq!(resolved, worktree_dir(&repo, "feat-a").to_string_lossy());
        assert!(Path::new(&resolved).join(".git").exists());
    }

    #[test]
    fn resolve_placeholders_expands_a_wrapped_placeholder_inside_cwd_text() {
        let repo = init_repo("wrapped-cwd");
        let store = Store::open_in_memory().unwrap();
        store
            .register_project("myproj", "", &repo.to_string_lossy(), "git")
            .unwrap();
        let mut cells = vec![cell_row(
            0,
            0,
            "s0",
            Some("<<ralphus:new-worktree/feat-wrapped?upstream=main>>/more/text"),
        )];
        let tasks = vec![task_row(0, Some("myproj"))];
        resolve_placeholders(&store, "squad-1", &mut cells, &tasks, &Context::new())
            .expect("materialize wrapped cwd");
        let resolved = cells[0].cwd.clone().expect("resolved cwd");
        assert_eq!(
            Path::new(&resolved),
            worktree_dir(&repo, "feat-wrapped")
                .join("more")
                .join("text")
        );
    }

    #[test]
    fn resolve_placeholders_preserves_unknown_wrapped_text_in_cwd() {
        let repo = init_repo("unknown-wrapped-cwd");
        let store = Store::open_in_memory().unwrap();
        store
            .register_project("myproj", "", &repo.to_string_lossy(), "git")
            .unwrap();
        let raw = "prefix/<<not-a-ralphus-placeholder>>/suffix";
        let mut cells = vec![cell_row(0, 0, "s0", Some(raw))];
        let tasks = vec![task_row(0, Some("myproj"))];

        resolve_placeholders(&store, "squad-1", &mut cells, &tasks, &Context::new())
            .expect("unknown placeholder text must pass through");

        assert_eq!(cells[0].cwd.as_deref(), Some(raw));
    }

    #[test]
    fn resolve_placeholders_reuses_one_worktree_across_cells() {
        // The same placeholder string repeated across two cells (as if two
        // tasks in one submission both referenced it) must materialize exactly
        // one worktree and resolve both cells to the identical real path.
        let repo = init_repo("dedupe");
        let store = Store::open_in_memory().unwrap();
        store
            .register_project("shared", "", &repo.to_string_lossy(), "git")
            .unwrap();
        let mut cells = vec![
            cell_row(
                0,
                0,
                "s0",
                Some("ralphus:new-worktree/feat-b?upstream=main"),
            ),
            cell_row(
                1,
                0,
                "s1",
                Some("ralphus:new-worktree/feat-b?upstream=main"),
            ),
        ];
        let tasks = vec![task_row(0, Some("shared")), task_row(1, Some("shared"))];
        resolve_placeholders(&store, "squad-1", &mut cells, &tasks, &Context::new())
            .expect("materialize");
        assert_eq!(cells[0].cwd, cells[1].cwd);

        // Only one worktree is registered with git for that branch.
        let list = git(&repo, &["worktree", "list", "--porcelain"]).unwrap();
        let count = list
            .lines()
            .filter(|l| l.starts_with("branch") && l.ends_with("feat-b"))
            .count();
        assert_eq!(
            count, 1,
            "expected exactly one worktree for the shared branch"
        );
    }

    #[test]
    fn resolve_placeholders_disambiguates_two_tasks_whose_branches_share_a_short_name() {
        // Regression for the RAL-239 smoke-test squad failure: two
        // independent tasks, each `ralphus:new-worktree/test-pr-submission-*`,
        // used to resolve to the identical worktree directory and race each
        // other's `git add`/`commit` in the same index (one cell exiting 1,
        // the other 128). Each must now resolve to its own worktree.
        let repo = init_repo("two-task-collide");
        let store = Store::open_in_memory().unwrap();
        store
            .register_project("ralphus", "", &repo.to_string_lossy(), "git")
            .unwrap();
        let mut cells = vec![
            cell_row(
                0,
                0,
                "work",
                Some("ralphus:new-worktree/test-pr-submission-a?upstream=main"),
            ),
            cell_row(
                1,
                0,
                "work",
                Some("ralphus:new-worktree/test-pr-submission-b?upstream=main"),
            ),
        ];
        let tasks = vec![task_row(0, Some("ralphus")), task_row(1, Some("ralphus"))];
        resolve_placeholders(&store, "squad-1", &mut cells, &tasks, &Context::new())
            .expect("materialize both");

        let resolved_a = cells[0].cwd.clone().expect("resolved a");
        let resolved_b = cells[1].cwd.clone().expect("resolved b");
        assert_ne!(
            resolved_a, resolved_b,
            "each task's branch must materialize its own worktree"
        );
        assert_eq!(
            git(Path::new(&resolved_a), &["symbolic-ref", "--short", "HEAD"])
                .unwrap()
                .trim(),
            "test-pr-submission-a"
        );
        assert_eq!(
            git(Path::new(&resolved_b), &["symbolic-ref", "--short", "HEAD"])
                .unwrap()
                .trim(),
            "test-pr-submission-b"
        );
    }

    #[test]
    fn resolve_placeholders_is_restart_safe() {
        // Simulate a restart: after the first resolution the cell's cwd is a
        // real path (as it would be, re-read from the store), so a second call
        // must be a no-op that neither errors nor recreates the worktree.
        let repo = init_repo("restart-safe");
        let store = Store::open_in_memory().unwrap();
        store
            .register_project("proj", "", &repo.to_string_lossy(), "git")
            .unwrap();
        let mut cells = vec![cell_row(
            0,
            0,
            "s0",
            Some("ralphus:new-worktree/feat-c?upstream=main"),
        )];
        let tasks = vec![task_row(0, Some("proj"))];
        resolve_placeholders(&store, "squad-1", &mut cells, &tasks, &Context::new())
            .expect("first resolution");
        let resolved = cells[0].cwd.clone().unwrap();

        std::fs::write(Path::new(&resolved).join("marker.txt"), "kept\n").unwrap();

        // Second call over freshly-loaded rows carrying the already-resolved cwd.
        resolve_placeholders(&store, "squad-1", &mut cells, &tasks, &Context::new())
            .expect("restart no-op");
        assert_eq!(cells[0].cwd.as_deref(), Some(resolved.as_str()));
        assert!(Path::new(&resolved).join("marker.txt").exists());
    }

    #[test]
    fn resolve_placeholders_expands_the_current_branch_sentinel_end_to_end() {
        // `?upstream=<<current_branch>>` must resolve to the project's
        // currently-checked-out branch and drive the new worktree's tracking —
        // the full path from placeholder cwd to materialized worktree.
        let repo = init_repo("resolve-sentinel-current");
        g(&repo, &["checkout", "-b", "base-branch"]);
        let store = Store::open_in_memory().unwrap();
        store
            .register_project("proj", "", &repo.to_string_lossy(), "git")
            .unwrap();
        let mut cells = vec![cell_row(
            0,
            0,
            "s0",
            Some("ralphus:new-worktree/feature?upstream=<<current_branch>>"),
        )];
        let tasks = vec![task_row(0, Some("proj"))];
        resolve_placeholders(&store, "squad-1", &mut cells, &tasks, &Context::new())
            .expect("resolve current_branch sentinel");
        let resolved = PathBuf::from(cells[0].cwd.clone().unwrap());
        assert_eq!(
            git(&resolved, &["symbolic-ref", "--short", "HEAD"])
                .unwrap()
                .trim(),
            "feature"
        );
        assert_eq!(
            git(
                &resolved,
                &[
                    "rev-parse",
                    "--abbrev-ref",
                    "--symbolic-full-name",
                    "@{upstream}",
                ],
            )
            .unwrap()
            .trim(),
            "base-branch",
            "the new branch must track the project's checked-out branch"
        );
    }
}
