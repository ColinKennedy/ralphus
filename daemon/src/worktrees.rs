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

use std::cell::RefCell;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::LazyLock;

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
    /// Configured `[machine.targets.*]` entries (RAL-355 Phase 2), loaded
    /// once per [`resolve_placeholders`] call and threaded down rather than
    /// re-read per cell. Carried on the context (not a separate parameter)
    /// purely so `provision_remote_with_targets` -- several calls deep --
    /// doesn't need its own dedicated parameter threaded through every
    /// intermediate function.
    pub(crate) targets:
        &'a std::collections::BTreeMap<String, crate::machine_targets::MachineTarget>,
    /// `(registered project name, bare upstream) -> already-fetched
    /// "<remote>/<branch>"`, computed by [`collect_remote_upstream_prefetch_targets`]
    /// and the caller's own fetch loop OUTSIDE the store lock this whole
    /// resolution pass otherwise runs under (RAL-<pending>) -- see
    /// [`resolve_placeholders_with_prefetch`]'s doc comment. A lookup miss
    /// (a `<<...>>` sentinel upstream, or a caller that didn't prefetch at
    /// all -- e.g. every existing test, and env-override placeholder
    /// expansion) falls back to fetching live, right where the fetch used to
    /// happen unconditionally, so correctness never depends on this cache
    /// being complete.
    pub(crate) prefetched_upstreams: &'a HashMap<(String, String), String>,
    /// Per-project-root `git worktree list --porcelain` snapshots
    /// (short-name -> branch, [`existing_task_worktree_branches`]'s shape),
    /// lazily populated by [`GitProjectStartupAdapter::resolve_placeholder`]
    /// and shared across every cell in one [`resolve_placeholders`] call --
    /// not just within one cell's own resolution the way
    /// [`ensure_worktree_with_existing`]'s doc comment describes.
    ///
    /// Querying this is O(worktree count) and, on a dev machine with
    /// hundreds accumulated, can take real wall-clock time -- paying for it
    /// once per DISTINCT PROJECT ROOT per submission (instead of once per
    /// branch) is the difference between that cost scaling with the
    /// project's total worktree count once, versus once per cell. Safe to
    /// share across cells because every new worktree this resolution pass
    /// itself creates is recorded back into the same map immediately after
    /// creation (see the catch-all branch's post-`ensure_worktree_with_existing`
    /// insert) -- so a later cell in this same call always sees an earlier
    /// one's freshly materialized worktree, exactly as a fresh git query
    /// would show it. `RefCell` because [`PlaceholderContext`] is `Copy` and
    /// threaded by value through a trait method.
    pub(crate) on_disk_worktrees: &'a RefCell<HashMap<PathBuf, HashMap<String, String>>>,
    /// This cell's own `cwd`, already resolved to a real path (or a plain
    /// literal, if it never was a placeholder) by the time
    /// [`materialize_env_overrides`] runs -- [`resolve_placeholders`] always
    /// resolves every cell's `cwd` before any cell (or its proof steps)
    /// dispatches (RAL-460). Used to resolve an `environment` value that
    /// links to `"cwd"` (see [`ralphus_core::schema::CellLinkTarget::Cwd`]).
    /// `None` only when there is no cwd to link to at all (irrelevant to
    /// [`GitProjectStartupAdapter::resolve_placeholder`], which is resolving
    /// `cwd` itself and so leaves this unset).
    pub(crate) cwd: Option<&'a str>,
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

/// In-process registry of `w/<short>` worktree directory slots already
/// claimed by an in-flight materialization but not yet visible via a live
/// `git worktree list --porcelain` query. [`resolve_task_worktree_dir_with_existing`]'s
/// collision avoidance (and [`resolve_squad_branch`]'s "is this branch
/// already checked out somewhere" check) used to be safe purely because the
/// daemon's global store lock serialized a submission's whole
/// materialization pass start to finish, INCLUDING the actual `git worktree
/// add` -- see [`resolve_task_worktree_dir_with_existing`]'s doc comment.
/// Splitting slot *decision* (fast, still made under the lock -- see
/// [`plan_local_worktree_jobs`]) from slot *creation* (slow, deliberately
/// run WITHOUT the lock so it can also run in parallel) broke that: a second
/// submission's locked planning pass can now run before the first
/// submission's `git worktree add` has actually landed on disk, and would
/// otherwise see a stale, still-empty `w/` directory and pick the same slot
/// for a different branch. This registry closes that gap -- a slot is
/// recorded the moment a decision is made to use it, before any git command
/// runs. Entries are never removed: once a worktree genuinely exists, a live
/// git query agrees anyway, so a stale entry is harmless.
static RESERVED_WORKTREE_SLOTS: LazyLock<
    parking_lot::Mutex<HashMap<PathBuf, HashMap<String, String>>>,
> = LazyLock::new(|| parking_lot::Mutex::new(HashMap::new()));

/// [`existing_task_worktree_branches`] merged with any slot this process has
/// already reserved for `root` (see [`RESERVED_WORKTREE_SLOTS`]) but not yet
/// materialized on disk. Use this wherever a materialization DECISION is
/// made; the plain query alone is only safe for read-only/display purposes.
fn existing_and_reserved_worktree_branches(root: &Path) -> HashMap<String, String> {
    let mut out = existing_task_worktree_branches(root);
    if let Some(reserved) = RESERVED_WORKTREE_SLOTS.lock().get(root) {
        for (short, branch) in reserved {
            out.entry(short.clone()).or_insert_with(|| branch.clone());
        }
    }
    out
}

/// Record that `plan`'s directory is now claimed for `branch` under `root`,
/// even though the worktree may not exist on disk yet -- see
/// [`RESERVED_WORKTREE_SLOTS`]'s doc comment. A no-op for a plan that reuses
/// an already-existing worktree (nothing new to protect).
fn reserve_worktree_slot(root: &Path, plan: &WorktreePlan, branch: &str) {
    if plan.already_exists {
        return;
    }
    let Some(short) = plan.wt.file_name().and_then(|n| n.to_str()) else {
        return;
    };
    RESERVED_WORKTREE_SLOTS
        .lock()
        .entry(root.to_path_buf())
        .or_default()
        .insert(short.to_string(), branch.to_string());
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
///
/// `existing` is a caller-supplied `git worktree list --porcelain` snapshot
/// rather than one freshly queried here -- see
/// [`ensure_worktree_with_existing`]'s doc comment for why: on a dev machine
/// with hundreds of worktrees this query alone can take real wall-clock
/// time, and [`GitProjectStartupAdapter::resolve_placeholder`] otherwise
/// pays for it twice per branch (once via [`resolve_squad_branch`], once via
/// this function) for no reason -- both reads happen back-to-back with no
/// worktree created in between, so sharing one snapshot between them is
/// exactly as fresh as querying twice.
fn resolve_task_worktree_dir_with_existing(
    root: &Path,
    branch: &str,
    existing: &HashMap<String, String>,
) -> PathBuf {
    let base = crate::short_paths::short_name(branch);
    let mut candidate = base.clone();
    let mut n = 2;
    loop {
        // Case-insensitive lookup: `w/<short>` and a branch's loose ref file
        // both live on the OS filesystem, which is case-*insensitive* on
        // Windows and on a default-configured macOS -- "RAL-428-x" and
        // "ral-428-x" are the SAME directory and the SAME ref there, even
        // though a plain `HashMap::get`/`==` sees them as different keys. A
        // case-sensitive comparison here let a placeholder that differed
        // only in case from an existing (possibly dirty, unowned) worktree
        // conclude its slot was free and collide straight into that
        // worktree's real on-disk directory and branch.
        match existing
            .iter()
            .find(|(short, _)| short.eq_ignore_ascii_case(&candidate))
        {
            None => break,
            Some((_, owner)) if owner.eq_ignore_ascii_case(branch) => break,
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

/// Whether `branch` names an existing remote-tracking ref (`refs/remotes/<branch>`).
///
/// Mirrors the condition [`branch_materialization`] uses to pick
/// [`BranchMaterialization::NewFromRemote`], but is deliberately checked
/// against the *remote* ref rather than that enum: once the first squad has
/// materialized `origin/foo`, a local branch by that literal name exists and
/// `branch_materialization` reports `ExistingLocal`, so it can no longer tell
/// a shared remote branch from an ordinary local one. `refs/remotes/` is
/// stable across both resolutions.
fn names_remote_tracking_branch(root: &Path, branch: &str) -> bool {
    branch.contains('/')
        && git(
            root,
            &["rev-parse", "--verify", &format!("refs/remotes/{branch}")],
        )
        .is_ok()
}

/// How many `<base>-2`, `-3`, ... branches one base branch may spawn before
/// resolution gives up. A ceiling, not a quota: reaching it means something
/// is allocating worktrees in a loop, and failing loudly beats silently
/// reusing another squad's branch (the exact bug this guards against).
const MAX_BRANCH_SUFFIX: usize = 1000;

/// The branch `squad_id` should actually use for a
/// `ralphus:new-worktree/<base_branch>` placeholder (RAL-337).
///
/// The placeholder names a *base* branch, but the branch a squad gets is not
/// necessarily that name. Resolving on the base name alone is what let a
/// second squad submitting the same task file land on the first squad's
/// branch — and therefore open a worktree that already contained the first
/// squad's finished commits, despite the placeholder asking for a *new*
/// worktree.
///
/// - **The owning squad keeps its branch.** A squad that already claimed a
///   branch in this family resolves back to that exact branch, so every cell
///   in the squad shares one worktree and `squad restart`/`squad retry`
///   return to the tree they left behind. This is the restart-safety property
///   [`ensure_worktree`] documents, now scoped to the squad that earned it
///   rather than granted to whoever asks next.
/// - **Any other squad gets a fresh branch.** The next free `<base>-2`,
///   `-3`, ... is claimed. Because that is a genuinely different branch name,
///   [`resolve_task_worktree_dir`]'s existing collision suffixing then gives
///   it its own `w/<short>-2` directory without any further help.
///
/// A branch counts as taken if a claim row holds it *or* a live worktree is
/// checked out on it. The second check is what handles worktrees that predate
/// this table (they have no row, but they are plainly still in use) and any
/// branch a user materialized by hand.
///
/// **A remote-tracking branch is exempt and always shared.** A placeholder
/// like `ralphus:new-worktree/origin/foo` names one specific, externally
/// owned branch — squads deliberately share that worktree and resync it to
/// new pushes (see [`resync_remote_tracking_branch`]). Allocating
/// `origin/foo-2` for the second squad would invent a local branch tracking
/// nothing, which is not what the placeholder asked for. Per-squad allocation
/// applies only to branches this daemon creates and owns.
fn resolve_squad_branch(
    store: &Store,
    project: &ProjectView,
    base_branch: &str,
    squad_id: &str,
    on_disk: &HashMap<String, String>,
) -> Result<String, String> {
    if names_remote_tracking_branch(Path::new(&project.path), base_branch) {
        return Ok(base_branch.to_string());
    }
    if let Some(existing) = store
        .task_worktree_claim_for_squad(&project.name, base_branch, squad_id)
        .map_err(|e| e.to_string())?
    {
        return Ok(existing);
    }
    let claims = store
        .task_worktree_claims(&project.name, base_branch)
        .map_err(|e| e.to_string())?;
    let claimed: HashSet<&str> = claims.iter().map(|c| c.branch.as_str()).collect();
    let occupied: HashSet<&str> = on_disk.values().map(String::as_str).collect();
    // Case-insensitive: see the matching comment on
    // `resolve_task_worktree_dir_with_existing` -- a branch differing only in
    // case from an already-claimed or already-checked-out one is the same
    // ref/directory on a case-insensitive filesystem, so `HashSet::contains`
    // (case-sensitive) must not be trusted to catch that collision.
    let is_taken = |candidate: &str| {
        claimed.iter().any(|c| c.eq_ignore_ascii_case(candidate))
            || occupied.iter().any(|o| o.eq_ignore_ascii_case(candidate))
    };

    let mut candidate = base_branch.to_string();
    let mut n = 2;
    while is_taken(&candidate) {
        if n > MAX_BRANCH_SUFFIX {
            return Err(format!(
                "could not find a free worktree branch for \"{base_branch}\" in project \
                 \"{}\" after {MAX_BRANCH_SUFFIX} attempts",
                project.name
            ));
        }
        candidate = format!("{base_branch}-{n}");
        n += 1;
    }
    store
        .record_task_worktree_claim(&project.name, base_branch, &candidate, squad_id)
        .map_err(|e| e.to_string())?;
    if candidate != base_branch {
        // ralphus[ignore-rlog-pair]: the Store-owning scheduler caller records this squad's structured workflow outcome.
        crate::rlog!(
            INFO,
            "ralphus [scheduler] worktree branch \"{base_branch}\" is already owned by another \
             squad; {squad_id} gets a new branch \"{candidate}\""
        );
    }
    Ok(candidate)
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

/// For a project registered with a remote (`clone_url` set via
/// `ralphus project git --url ...`), resolve a bare `?upstream=<branch>`
/// value (no `<<...>>` sentinel, no explicit `<remote>/<branch>` prefix)
/// against that remote -- fetching it fresh -- instead of against whatever
/// the shared, locally-registered `root` checkout happens to have on disk
/// under that branch name right now.
///
/// Without this, a registered project's bare upstream resolved purely
/// locally (`refs/heads/<upstream>` in `root`), which reflects whatever a
/// concurrent `git checkout`/commit in that same shared directory last left
/// there -- not necessarily the branch the submitter meant by naming a
/// remote-backed project's upstream. Rewriting to the explicit
/// `<remote>/<branch>` form here makes every downstream step
/// ([`branch_materialization`], [`set_explicit_upstream`],
/// [`resync_remote_tracking_branch`]) treat a bare `?upstream=staging`
/// exactly as if the submitter had written `?upstream=origin/staging`
/// themselves -- reusing their already-correct remote-tracking handling
/// rather than adding a parallel code path.
///
/// A no-op for a project with no `clone_url` (a purely local repo has
/// nothing to fetch from, so its bare upstream stays a local branch lookup)
/// and for an `upstream` that already names its remote explicitly.
///
/// `pub(crate)` (not just used from [`GitProjectStartupAdapter::resolve`]
/// below): [`crate::reviews`] applies the same rule to a `[[review]]`
/// block's own declared `upstream` field, which is a value the submitter
/// writes directly rather than a cell `cwd`'s `?upstream=` -- same
/// registered-remote-vs-shared-checkout ambiguity, so it needs the same fix.
pub(crate) fn resolve_registered_remote_upstream(
    root: &Path,
    project: &ProjectView,
    upstream: &str,
) -> Result<String, String> {
    if project.clone_url.is_none() || upstream.contains('/') {
        return Ok(upstream.to_string());
    }
    let cfg = crate::config::resolve_forge(root);
    let remote = crate::forge::resolve_remote_name(root, upstream, &cfg);
    if git(root, &["remote", "get-url", &remote]).is_err() {
        return Ok(upstream.to_string());
    }
    let refspec = format!("+refs/heads/{upstream}:refs/remotes/{remote}/{upstream}");
    git(root, &["fetch", &remote, &refspec]).map_err(|e| {
        format!(
            "project \"{}\" is registered with a remote, but \"?upstream={upstream}\" could not \
             be fetched from \"{remote}\": {e}",
            project.name
        )
    })?;
    Ok(format!("{remote}/{upstream}"))
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
        "\"?upstream=<<default>>\" could not resolve a default branch in {}: no remote has a \
         configured symbolic HEAD (no remote's `refs/remotes/<remote>/HEAD` is set), so there \
         is no default branch for \"<<default>>\" to resolve to. Fix: run \
         `git remote set-head origin --auto` in this repository, or use a literal branch name \
         in the upstream field instead",
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
///
/// Does *not* itself record the no-new-commits guard's durable baseline
/// marker (`ralphus.<branch>.baseline`) -- that used to happen here, mirroring
/// whichever ref this function just resolved, but a ref *name* is a moving
/// target: `branch.<branch>.remote`/`.merge` (i.e. `@{upstream}`) is exactly
/// what `git push -u`/`--set-upstream` overwrites, and a remote-tracking ref
/// used as the marker can itself be advanced mid-run by any push or fetch
/// that touches it (including a `finalize` cell pushing its own commit
/// straight onto what `git status` calls "your branch's upstream," which is
/// actually the shared base branch here). Either way the guard would then
/// read the branch's own real work as "already accounted for." The caller
/// ([`ensure_worktree_with_existing`]) instead freezes the marker to a
/// resolved commit SHA, once, *after* this function returns and the branch
/// has been resynced -- see [`crate::reviews::set_worktree_commit_baseline`].
/// Guards every `git config` WRITE this module makes to a worktree's branch
/// tracking (`set_explicit_upstream` and `freeze_commit_baseline`, below).
/// A worktree's `.git/config` is the SAME physical file shared by every
/// other worktree of the same project (worktrees each get their own
/// index/HEAD, but not their own config) -- two `git config <key> <value>`
/// invocations against different worktrees of the same repo, run at the same
/// instant, race on git's own `.git/config.lock` and one of them fails
/// outright ("could not lock config file: File exists"), rather than
/// queuing. That never mattered while worktree materialization was fully
/// serial; it does now that [`execute_local_worktree_jobs`] runs several
/// worktrees' materialization concurrently, potentially against the same
/// project root. A single global lock (not per-root) is simpler than
/// tracking one per project and costs nothing worth measuring -- the
/// critical section is a couple of millisecond-scale `git config` calls, not
/// the actual (slow) checkout. Every call `execute_worktree_plan` makes that
/// can write to `.git/config` -- including `git worktree add`'s own implicit
/// tracking-setup side effect for a newly created branch -- must be covered:
/// `execute_worktree_plan` passes `--no-track` to every `worktree add -b`
/// call for exactly this reason, so the *only* config writes are the two
/// explicit, lock-guarded ones here.
static WORKTREE_CONFIG_LOCK: LazyLock<parking_lot::Mutex<()>> =
    LazyLock::new(|| parking_lot::Mutex::new(()));

fn set_explicit_upstream(wt: &Path, branch: &str, upstream: &str) -> Result<(), String> {
    let _guard = WORKTREE_CONFIG_LOCK.lock();
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

/// Freeze the no-new-commits guard's durable baseline marker
/// (`ralphus.<branch>.baseline`) to `branch`'s current `@{upstream}`,
/// resolved to a commit SHA right now. Called once [`set_explicit_upstream`]
/// and [`resync_remote_tracking_branch`] have both already run, so the
/// snapshot reflects the branch's real starting line for this
/// materialization -- any commits the resync just fetched and rebased in are
/// included (they're genuinely "already there" before this run's own cells
/// do anything), but nothing that pushes or fetches into that same ref
/// *afterward*, while cells are running, can move it. A no-op (not an error)
/// if `@{upstream}` doesn't resolve, matching this marker's existing
/// best-effort, fall-back-to-live-`@{upstream}` contract in
/// [`crate::reviews::workspace_baseline_ref`].
fn freeze_commit_baseline(wt: &Path, branch: &str) {
    let Ok(upstream) = git(
        wt,
        &[
            "rev-parse",
            "--abbrev-ref",
            "--symbolic-full-name",
            "@{upstream}",
        ],
    ) else {
        return;
    };
    // `set_worktree_commit_baseline` writes `ralphus.<branch>.baseline` to
    // the same shared `.git/config` `set_explicit_upstream` writes to --
    // needs the same `WORKTREE_CONFIG_LOCK` for the same reason (see that
    // static's doc comment); a worktree's `freeze_commit_baseline` call can
    // otherwise race a *different* worktree's concurrent
    // `set_explicit_upstream` call on git's own `.git/config.lock`.
    let _guard = WORKTREE_CONFIG_LOCK.lock();
    if let Err(e) = crate::reviews::set_worktree_commit_baseline(wt, upstream.trim()) {
        // ralphus[ignore-rlog-pair]: worktree helper has no Store; caller records review workflow outcomes
        crate::rlog!(
            WARNING,
            "ralphus: could not freeze commit baseline for branch '{branch}' at {}: {e}",
            wt.display()
        );
    }
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
/// slashes included — and a new local branch is forked from the resolved
/// `upstream` ref itself, **never from `root`'s current `HEAD`**: whatever
/// the shared project checkout happens to have checked out at materialization
/// time must never leak into a freshly created branch's history. When that
/// literal name *looks* like `<remote>/<branch>` but no matching
/// remote-tracking ref exists, a warning is emitted so the fallback is
/// explicit rather than silently guessed. Either way, `upstream` -- not the
/// branch's own name or `root`'s `HEAD` -- decides both what the branch
/// actually starts from and what tracking gets configured; a review worktree
/// tracking a remote keeps up with pushes to that branch over the life of the
/// project via the resync described above.
pub fn ensure_worktree(root: &Path, branch: &str, upstream: &str) -> Result<PathBuf, String> {
    ensure_worktree_with_existing(
        root,
        branch,
        upstream,
        &existing_task_worktree_branches(root),
    )
}

/// Install or remove the RAL-445 co-author `prepare-commit-msg` hook for
/// `root`'s project, per its resolved `.ralphus.toml` `[commits]
/// add_coauthor` (`crate::config::load_commit_config`). Failure (e.g. a
/// read-only hooks directory) is logged and swallowed rather than
/// propagated -- a hook sync must never block a squad's actual work, and
/// re-running this on the project's next materialization retries it anyway.
fn sync_coauthor_hook_best_effort(root: &Path) {
    let enabled = crate::config::load_commit_config(root).add_coauthor();
    if let Err(e) = crate::git_hooks::sync_coauthor_hook(root, enabled) {
        // ralphus[ignore-rlog-pair]: worktree setup helper without access to Store for Cartographer logging
        crate::rlog!(
            WARNING,
            "ralphus [worktrees] could not sync co-author hook for {}: {e}",
            root.display()
        );
    }
}

/// Like [`ensure_worktree`], but `existing` is a caller-supplied `git
/// worktree list --porcelain` snapshot instead of one freshly queried here.
///
/// [`GitProjectStartupAdapter::resolve_placeholder`] already calls
/// [`resolve_squad_branch`] for the same branch immediately before this, and
/// that also needs the on-disk worktree listing -- querying it twice (once
/// there, once here via [`resolve_task_worktree_dir`]) doubles a git call
/// that's O(worktree count) and can take real wall-clock time on a dev
/// machine with hundreds of them accumulated, all spent while the caller
/// holds the daemon's single global `Mutex<Store>`. No worktree is created
/// between those two reads, so one shared snapshot is exactly as fresh as
/// querying twice.
/// The decision [`ensure_worktree_with_existing`] makes about how to
/// materialize `branch`'s worktree, split out so it can be made (fast,
/// local-only, safe under the store lock) separately from actually doing it
/// (slow, safe to run without the lock and concurrently with another
/// root/branch's plan) -- see [`plan_local_worktree_jobs`].
struct WorktreePlan {
    wt: PathBuf,
    materialization: BranchMaterialization,
    already_exists: bool,
}

/// The fast half of [`ensure_worktree_with_existing`]: everything needed to
/// know WHERE `branch`'s worktree will live and HOW it must be created,
/// using only local git plumbing (`branch_materialization`) and the
/// caller-supplied on-disk snapshot -- no network, no checkout. Safe to call
/// while holding the store lock.
fn plan_worktree(
    root: &Path,
    branch: &str,
    existing: &HashMap<String, String>,
) -> Result<WorktreePlan, String> {
    let materialization = branch_materialization(root, branch)?;
    let wt = resolve_task_worktree_dir_with_existing(root, branch, existing);
    let already_exists = wt.join(".git").exists();
    Ok(WorktreePlan {
        wt,
        materialization,
        already_exists,
    })
}

/// The slow half of [`ensure_worktree_with_existing`]: perform `plan`'s git
/// work. An already-existing worktree only needs resyncing
/// (`set_explicit_upstream` + `resync_remote_tracking_branch`, a real `git
/// fetch`+rebase when it tracks a remote); a new one is created with `git
/// worktree add` first. No store access, so this is safe to call with no
/// lock held, and safe to call concurrently with another call for a
/// DIFFERENT `root`/`branch` -- see [`plan_worktree`]'s doc comment for why
/// the decision itself must already be final (and, for a brand-new slot,
/// already reserved via [`reserve_worktree_slot`]) before this runs.
fn execute_worktree_plan(
    root: &Path,
    branch: &str,
    upstream: &str,
    plan: &WorktreePlan,
) -> Result<PathBuf, String> {
    sync_coauthor_hook_best_effort(root);
    if plan.already_exists {
        set_explicit_upstream(&plan.wt, branch, upstream)?;
        resync_remote_tracking_branch(&plan.wt)?;
        freeze_commit_baseline(&plan.wt, branch);
        return Ok(plan.wt.clone());
    }
    if let Some(parent) = plan.wt.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("could not create worktree parent directory: {e}"))?;
    }
    let wt_str = plan.wt.to_string_lossy().to_string();
    match &plan.materialization {
        BranchMaterialization::ExistingLocal => {
            preflight_worktree_budget(root, &plan.wt, branch, path_budget_limit())?;
            git(root, &["worktree", "add", &wt_str, branch])?;
        }
        BranchMaterialization::NewFromRemote { remote_ref } => {
            preflight_worktree_budget(root, &plan.wt, remote_ref, path_budget_limit())?;
            // `--no-track`, not `--track`: `set_explicit_upstream` below sets
            // the real tracking config anyway (under `WORKTREE_CONFIG_LOCK`),
            // and git's own implicit auto-tracking setup for a new branch
            // writes to the SAME shared `.git/config` this worktree's
            // siblings may be concurrently touching -- unlike the explicit
            // call, this implicit write has no lock protecting it, and races
            // on git's own `.git/config.lock` under real concurrency (see
            // `WORKTREE_CONFIG_LOCK`'s doc comment).
            git(
                root,
                &[
                    "worktree",
                    "add",
                    "--no-track",
                    "-b",
                    branch,
                    &wt_str,
                    remote_ref,
                ],
            )?;
        }
        BranchMaterialization::NewFromHead { warning } => {
            if let Some(warning) = warning {
                // ralphus[ignore-rlog-pair]: this low-level helper has no Store; its Store-owning caller records the structured workflow outcome
                crate::rlog!(WARNING, "ralphus [scheduler] {warning}");
            }
            // Branch explicitly from the resolved `upstream` ref -- never
            // implicit `HEAD` -- so this new branch's content always reflects
            // what the submitter named, not whatever the shared `root`
            // checkout happens to have checked out right now.
            preflight_worktree_budget(root, &plan.wt, upstream, path_budget_limit())?;
            // `--no-track`: see the `NewFromRemote` arm above -- `upstream`
            // being a plain local branch still triggers git's own implicit,
            // unlocked auto-tracking setup by default (`branch.autoSetupMerge`),
            // which races the same way.
            git(
                root,
                &[
                    "worktree",
                    "add",
                    "--no-track",
                    "-b",
                    branch,
                    &wt_str,
                    upstream,
                ],
            )?;
        }
    }
    set_explicit_upstream(&plan.wt, branch, upstream)?;
    resync_remote_tracking_branch(&plan.wt)?;
    freeze_commit_baseline(&plan.wt, branch);
    Ok(plan.wt.clone())
}

pub fn ensure_worktree_with_existing(
    root: &Path,
    branch: &str,
    upstream: &str,
    existing: &HashMap<String, String>,
) -> Result<PathBuf, String> {
    let plan = plan_worktree(root, branch, existing)?;
    execute_worktree_plan(root, branch, upstream, &plan)
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
    targets: &'a std::collections::BTreeMap<String, crate::machine_targets::MachineTarget>,
    prefetched_upstreams: &'a HashMap<(String, String), String>,
    on_disk_worktrees: &'a RefCell<HashMap<PathBuf, HashMap<String, String>>>,
) -> PlaceholderContext<'a> {
    PlaceholderContext {
        squad_id,
        task_idx: cell.task_idx,
        cell_idx: cell.idx,
        task_name: &cell.task_name,
        cell_id: &cell.cell_id,
        machine: cell.machine.as_deref(),
        targets,
        prefetched_upstreams,
        on_disk_worktrees,
        // This context resolves the cell's own `cwd` placeholder, so there is
        // no already-known `cwd` to link to yet.
        cwd: None,
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
        maximum_context: None,
        auto_compact_threshold: None,
        maximum_tool_output_tokens: None,
        upstream: None,
        machine: ctx.machine.map(str::to_string),
        share_session: false,
        maximum_timeout_sec: None,
        task_maximum_timeout_sec: None,
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
        //
        // Wrapped with cell + placeholder context here, at the source, rather
        // than relying on a caller to add it: `resolve_placeholders_inner`
        // does wrap its own cwd-resolution call, but `materialize_env_overrides`
        // (RAL-447) resolves env-override placeholders through this same
        // adapter without any equivalent wrap, so this is the one place that
        // reliably preserves which cell and placeholder a resolution failure
        // came from regardless of caller.
        let upstream = resolve_upstream(Path::new(&project.path), upstream).map_err(|e| {
            format!(
                "cell '{}': could not resolve upstream for \"{placeholder}\": {e}",
                ctx.cell_id
            )
        })?;
        // RAL-337: the placeholder names a *base* branch; which branch this
        // squad actually gets depends on whether another squad already owns
        // it. Without this, a resubmission of the same task file resolves to
        // the first squad's branch and inherits its finished commits.
        //
        // The on-disk snapshot this needs (and `ensure_worktree_with_existing`
        // below needs again) is queried at most once per project root for
        // this whole submission, via `ctx.on_disk_worktrees` -- not once per
        // branch -- see `PlaceholderContext::on_disk_worktrees`'s doc comment.
        let root_key = Path::new(&project.path).to_path_buf();
        if !ctx.on_disk_worktrees.borrow().contains_key(&root_key) {
            let snapshot = existing_and_reserved_worktree_branches(&root_key);
            ctx.on_disk_worktrees
                .borrow_mut()
                .insert(root_key.clone(), snapshot);
        }
        let branch = {
            let all = ctx.on_disk_worktrees.borrow();
            let on_disk = all.get(&root_key).expect("populated just above");
            resolve_squad_branch(store, project, branch, ctx.squad_id, on_disk)
        }
        .map_err(|e| {
            format!(
                "cell '{}': could not allocate a worktree branch for \"{placeholder}\": {e}",
                ctx.cell_id
            )
        })?;
        let branch = branch.as_str();
        let resolved = match ctx.machine {
            Some(machine) if !machine.trim().is_empty() => provision_remote_with_targets(
                store,
                machine,
                project,
                branch,
                &upstream,
                &synthetic_cell_row(ctx),
                ctx.squad_id,
                ctx.targets,
            )?,
            _ => {
                // RAL-<pending>: a registered (remote-backed) project's bare
                // upstream must resolve against that remote, never against
                // whatever the shared `project.path` checkout happens to have
                // on disk -- see `resolve_registered_remote_upstream`. Prefer
                // an already-fetched result from `ctx.prefetched_upstreams`
                // (computed by the caller OUTSIDE the store lock this whole
                // function runs under) over fetching live right here, which
                // would hold that lock for as long as the network fetch
                // takes -- see `resolve_placeholders_with_prefetch`'s doc
                // comment. Falling back to a live fetch on a cache miss keeps
                // this correct even when the caller didn't prefetch at all.
                let upstream = match ctx
                    .prefetched_upstreams
                    .get(&(project.name.clone(), upstream.clone()))
                {
                    Some(resolved) => resolved.clone(),
                    None => resolve_registered_remote_upstream(
                        Path::new(&project.path),
                        project,
                        &upstream,
                    )?,
                };
                let materialized = {
                    let all = ctx.on_disk_worktrees.borrow();
                    let on_disk = all.get(&root_key).expect("populated above");
                    ensure_worktree_with_existing(
                        Path::new(&project.path),
                        branch,
                        &upstream,
                        on_disk,
                    )
                }
                .map_err(|e| {
                    format!(
                        "cell '{}': could not materialize worktree for \"{placeholder}\": {e}",
                        ctx.cell_id
                    )
                })?;
                // Record this branch's (new-or-reused) worktree back into the
                // shared snapshot so a LATER cell in this same submission
                // sees it without a fresh `git worktree list` -- exactly
                // what a live re-query would show it, since this is the only
                // thing that could have changed the on-disk state since the
                // snapshot was taken.
                ctx.on_disk_worktrees
                    .borrow_mut()
                    .get_mut(&root_key)
                    .expect("populated above")
                    .insert(
                        crate::short_paths::short_name(branch).to_string(),
                        branch.to_string(),
                    );
                materialized.to_string_lossy().into_owned()
            }
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
        let Some(close) = ralphus_core::schema::placeholder_close(raw, body_start) else {
            out.push_str(&raw[open..]);
            return Ok(out);
        };
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

/// Resolve every `environment` value for one cell/proof-step scope,
/// including `"<<ralphus:link/<field>>>"` linked fields (RAL-460) in
/// dependency order: a value that links to another `environment` entry is
/// only resolved once that entry's own value is fully resolved, however many
/// links deep the chain goes -- never in one hardcoded hop. A value that
/// isn't a linked field falls through to the existing worktree-placeholder
/// expansion ([`resolve_placeholder_text_for_project`]) unchanged, so a plain
/// literal or an embedded `ralphus:new-worktree/...` placeholder resolves
/// exactly as it always has.
pub(crate) fn materialize_env_overrides(
    store: &Store,
    ctx: PlaceholderContext<'_>,
    env: &BTreeMap<String, String>,
) -> Result<BTreeMap<String, String>, String> {
    let project_name = store
        .task_project_at(ctx.squad_id, ctx.task_idx)
        .map_err(|e| e.to_string())?;
    let mut cache = HashMap::new();
    let mut resolved: BTreeMap<String, String> = BTreeMap::new();
    let mut resolving: HashSet<String> = HashSet::new();
    for key in env.keys() {
        resolve_env_entry(
            store,
            project_name.as_deref(),
            env,
            key,
            ctx,
            &mut cache,
            &mut resolved,
            &mut resolving,
        )?;
    }
    Ok(resolved)
}

/// Resolve one `environment` entry (`key`) of `env`, memoizing into
/// `resolved` and recursing (with cycle detection via `resolving`) when its
/// value links to another entry in the same `env` map. See
/// [`materialize_env_overrides`].
#[allow(clippy::too_many_arguments)]
fn resolve_env_entry(
    store: &Store,
    project_name: Option<&str>,
    env: &BTreeMap<String, String>,
    key: &str,
    ctx: PlaceholderContext<'_>,
    cache: &mut HashMap<String, String>,
    resolved: &mut BTreeMap<String, String>,
    resolving: &mut HashSet<String>,
) -> Result<String, String> {
    if let Some(value) = resolved.get(key) {
        return Ok(value.clone());
    }
    if !resolving.insert(key.to_string()) {
        return Err(format!(
            "cell '{}': environment \"{key}\" is part of a circular linked-field chain",
            ctx.cell_id
        ));
    }
    let raw = env
        .get(key)
        .expect("key came from this same env map's own keys()");
    let value = match ralphus_core::schema::parse_wrapped_cell_link(raw) {
        Ok(None) => match project_name {
            Some(project_name) => {
                resolve_placeholder_text_for_project(store, project_name, raw, ctx, cache)?
            }
            None => raw.clone(),
        },
        Ok(Some(link)) => {
            if let Some(suffix) = link.suffix {
                if !ralphus_core::schema::is_valid_cell_link_suffix(suffix) {
                    return Err(format!(
                        "cell '{}': environment \"{key}\" has an invalid link suffix {suffix:?}",
                        ctx.cell_id
                    ));
                }
            }
            let base = match ralphus_core::schema::parse_cell_link_target(link.field) {
                Some(ralphus_core::schema::CellLinkTarget::Cwd) => {
                    ctx.cwd.map(str::to_string).ok_or_else(|| {
                        format!(
                            "cell '{}': environment \"{key}\" links to \"cwd\", which has no \
                             value in this scope",
                            ctx.cell_id
                        )
                    })?
                }
                Some(ralphus_core::schema::CellLinkTarget::Environment(target_key)) => {
                    if !env.contains_key(target_key) {
                        return Err(format!(
                            "cell '{}': environment \"{key}\" links to \
                             \"environment.{target_key}\", which does not exist",
                            ctx.cell_id
                        ));
                    }
                    resolve_env_entry(
                        store,
                        project_name,
                        env,
                        target_key,
                        ctx,
                        cache,
                        resolved,
                        resolving,
                    )?
                }
                None => {
                    return Err(format!(
                        "cell '{}': environment \"{key}\" links to unsupported field \"{}\"",
                        ctx.cell_id, link.field
                    ));
                }
            };
            match link.suffix {
                Some(suffix) => Path::new(&base).join(suffix).to_string_lossy().into_owned(),
                None => base,
            }
        }
        Err(_) => {
            return Err(format!(
                "cell '{}': environment \"{key}\" is a malformed linked-field sentinel",
                ctx.cell_id
            ));
        }
    };
    resolving.remove(key);
    resolved.insert(key.to_string(), value.clone());
    Ok(value)
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
/// Loads the configured [`crate::machine_targets::MachineTarget`]s as a
/// parameter rather than reading them itself, so the caller controls
/// exactly when/how often `machine_targets::load_machine_targets` (which
/// reads real process environment variables --
/// `RALPHUS_CONFIG_HOME`/`RALPHUS_CONFIGURATION_PATH`) actually runs.
/// [`resolve_placeholders`] loads it once per call and threads it down via
/// `PlaceholderContext::targets`; tests that need a specific
/// `[machine.targets.*]` entry construct a `BTreeMap` directly and call
/// this function through [`resolve_placeholders_with_targets`] instead,
/// since they cannot safely override those environment variables in-process
/// (this workspace forbids `unsafe_code`, so `std::env::set_var` is
/// unavailable) — the same rationale `agent_profiles`'s own
/// `load_profiles_for_path_with` documents for its `configuration_path_env`
/// parameter.
#[allow(clippy::too_many_arguments)]
fn provision_remote_with_targets(
    store: &Store,
    machine: &str,
    project: &crate::store::ProjectView,
    branch: &str,
    upstream: &str,
    cell: &CellRow,
    squad_id: &str,
    targets: &std::collections::BTreeMap<String, crate::machine_targets::MachineTarget>,
) -> Result<String, String> {
    let provider = crate::remote_runner::provider_from_store(store, machine)
        .map_err(|e| format!("cell '{}': {e}", cell.cell_id))?
        .ok_or_else(|| {
            // `provision_remote_with_targets` is only called for a non-empty
            // machine, so a local resolution here means the value changed
            // under us.
            format!(
                "cell '{}': machine \"{machine}\" resolved to the local host",
                cell.cell_id
            )
        })?;
    // Only git has a clone URL to resolve; every other kind gets `None` and
    // the provider decides how to obtain the source.
    let url = if project.vcs == "git" {
        Some(project.clone_url.clone().ok_or_else(|| {
            format!(
                "cell '{}': git project {:?} has no registered clone URL; re-register it with `ralphus project git --name {} --path <local-path> --url <clone-url>` before using it on a remote machine",
                cell.cell_id, project.name, project.name
            )
        })?)
    } else {
        None
    };
    // RAL-355 Phase 2/4: a git project's remote workspace needs a durable
    // `remote_root` to provision persistent storage under -- refused here,
    // before ever dispatching to the provider, the same "fail before
    // provider dispatch" shape the missing-clone-URL check above already
    // uses, rather than letting the provider discover the gap at its own
    // runtime and report a less actionable error.
    let target = crate::machine_targets::find_by_machine(targets, machine);
    let remote_root = if project.vcs == "git" {
        let target = target.ok_or_else(|| {
            format!(
                "cell '{}': machine {machine:?} has no [machine.targets.*] entry configured with a remote_root; register one before provisioning a git project there",
                cell.cell_id
            )
        })?;
        Some(target.remote_root.clone())
    } else {
        None
    };
    let req = crate::remote_runner::ProvisionRequest {
        project: project.name.clone(),
        source: crate::remote_runner::WorkspaceSource {
            kind: project.vcs.clone(),
            url,
            branch: Some(branch.to_string()),
            upstream: Some(upstream.to_string()),
        },
        squad_id: squad_id.to_string(),
        cell_id: cell.cell_id.clone(),
        remote_root,
        runner: target.map(|target| crate::remote_runner::TargetRunnerConfig {
            mode: match target.runner_mode {
                crate::machine_targets::RunnerMode::Installed => "installed",
                crate::machine_targets::RunnerMode::Upload => "upload",
            }
            .to_string(),
            command: target.runner_command.clone(),
            artifacts: target.runner_artifacts.clone(),
            remote_root: target.remote_root.clone(),
        }),
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
    resolve_placeholders_with_prefetch(store, squad_id, cells, tasks, &HashMap::new(), parent)
}

/// Like [`resolve_placeholders`], but `prefetched_upstreams` supplies
/// `(registered project name, bare upstream) -> "<remote>/<branch>"` results
/// the caller already fetched -- see
/// [`collect_remote_upstream_prefetch_targets`]'s doc comment for how to
/// build this map, and why: `resolve_placeholders` runs entirely while its
/// caller (`execute_squad_inner`, `derive_reviews`) holds the daemon's single
/// global `Mutex<Store>` (RAL-<pending>'s [`GitProjectStartupAdapter::resolve_placeholder`]
/// added the first git operation in this whole call graph that hits the
/// network -- `resolve_registered_remote_upstream`'s `git fetch` -- rather
/// than staying local like every other step here). A dead or slow remote can
/// stall that fetch for the full `GIT_TIMEOUT`, and while it's stalled every
/// other request queues behind the same lock, so the entire daemon API
/// freezes for as long as the fetch takes -- not just this one squad. Doing
/// every such fetch up front, before the lock is even taken, and passing the
/// results in here removes that fetch from the locked critical path
/// entirely. A cache miss (a `<<...>>` sentinel upstream that hadn't expanded
/// to a concrete branch name yet when the caller collected targets, or a
/// caller that didn't prefetch at all -- e.g. [`resolve_placeholders`] above,
/// used by every existing test) falls back to fetching live right here,
/// under whatever lock the caller holds, exactly as it always has: this
/// parameter only ever makes the common case faster, never changes what a
/// given input resolves to.
pub fn resolve_placeholders_with_prefetch(
    store: &Store,
    squad_id: &str,
    cells: &mut [CellRow],
    tasks: &[TaskRow],
    prefetched_upstreams: &HashMap<(String, String), String>,
    parent: &Context,
) -> Result<(), String> {
    resolve_placeholders_with_full_prefetch(
        store,
        squad_id,
        cells,
        tasks,
        prefetched_upstreams,
        &HashMap::new(),
        parent,
    )
}

/// Like [`resolve_placeholders_with_prefetch`], but `prefetched_worktrees`
/// additionally supplies already-materialized LOCAL worktree paths, keyed
/// exactly like [`placeholder_cache_key`] -- see [`plan_local_worktree_jobs`]
/// and [`execute_local_worktree_jobs`] for how to build this map, and why:
/// running the actual `git worktree add`/`fetch`/rebase for every cell
/// OUTSIDE the store lock, in parallel, is what turns a squad with N
/// independent branches from N sequential git operations -- held under the
/// daemon's single global lock, so it stalls the *entire* daemon meanwhile
/// (board reads, every other squad's dispatch, everything) -- into one
/// bounded-concurrency batch that finishes in roughly the time of the
/// slowest single worktree. A cache miss (any cell the prefetch pass didn't
/// cover, e.g. a `machine`-provisioned one, or one that failed to
/// materialize during prefetch) falls back to resolving live, under this
/// call's lock, exactly as before -- this parameter only ever makes the
/// common case faster, never changes what a given input resolves to.
pub fn resolve_placeholders_with_full_prefetch(
    store: &Store,
    squad_id: &str,
    cells: &mut [CellRow],
    tasks: &[TaskRow],
    prefetched_upstreams: &HashMap<(String, String), String>,
    prefetched_worktrees: &HashMap<String, String>,
    parent: &Context,
) -> Result<(), String> {
    let span = otel::start_span("scheduler.resolve_worktrees", parent, SpanKind::Internal);
    span.set_attribute("squad_id", squad_id.to_string());
    // RAL-355 Phase 2: loaded once per call (not per cell) and threaded down
    // via `PlaceholderContext::targets` -- see `provision_remote_with_targets`'s
    // doc comment for why the loading itself isn't pushed further down.
    let targets = match crate::machine_targets::load_machine_targets() {
        Ok(t) => t,
        Err(e) => {
            span.set_status(Status::error(e.clone()));
            crate::rlog!(
                WARNING,
                "ralphus [scheduler] squad {squad_id} could not load machine targets: {e}"
            );
            return Err(e);
        }
    };
    match resolve_placeholders_inner(
        store,
        squad_id,
        cells,
        tasks,
        &targets,
        prefetched_upstreams,
        prefetched_worktrees,
    ) {
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

/// Every `(registered project, bare upstream)` pair among `cells`' worktree
/// placeholders that [`GitProjectStartupAdapter::resolve_placeholder`] would
/// otherwise call [`resolve_registered_remote_upstream`] for -- i.e. a
/// placeholder naming a project registered with a remote (`clone_url` set)
/// and an already-literal upstream (no `<<...>>` sentinel, no explicit
/// `<remote>/<branch>` prefix). Read-only over `store` and cheap (no git
/// subprocess, just parsing + a project lookup), so it's safe to call while
/// holding whatever lock guards `store` -- the caller's job is to then DROP
/// that lock and run the actual `git fetch` for each result (call
/// [`resolve_registered_remote_upstream`] directly) before calling
/// [`resolve_placeholders_with_prefetch`] with the results, keyed by
/// `(project.name.clone(), upstream)`. See
/// [`resolve_placeholders_with_prefetch`]'s doc comment for why this
/// two-phase split exists.
///
/// A `<<...>>` sentinel upstream is skipped here on purpose: expanding it
/// (`resolve_upstream`) needs local git state read from the project root, so
/// the concrete branch name it resolves to genuinely isn't known until
/// [`resolve_placeholders_with_prefetch`]'s own locked pass runs -- that
/// expansion itself stays local/cheap (RAL-258), so leaving it there costs
/// nothing. Only the plain-literal case (the common one) is collected here.
#[must_use]
pub fn collect_remote_upstream_prefetch_targets(
    store: &Store,
    cells: &[CellRow],
    tasks: &[TaskRow],
) -> Vec<(ProjectView, String)> {
    let task_projects: HashMap<i64, Option<&str>> = tasks
        .iter()
        .map(|t| (t.idx, t.project.as_deref()))
        .collect();
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for cell in cells {
        let Some(cwd) = cell.cwd.as_deref() else {
            continue;
        };
        let mut placeholders = Vec::new();
        if classify_placeholder(cwd).ok().flatten().is_some() {
            placeholders.push(cwd);
        }
        placeholders.extend(
            ralphus_core::schema::text_placeholders(cwd)
                .into_iter()
                .filter(|body| ralphus_core::schema::parse_worktree_placeholder(body).is_some()),
        );
        for placeholder in placeholders {
            let Some(upstream) =
                ralphus_core::schema::parse_worktree_placeholder_upstream(placeholder)
            else {
                continue;
            };
            if upstream.starts_with("<<") || upstream.contains('/') {
                continue;
            }
            let Some(project_name) = task_projects.get(&cell.task_idx).copied().flatten() else {
                continue;
            };
            if !seen.insert((project_name.to_string(), upstream.to_string())) {
                continue;
            }
            let Ok(Some(project)) = store.resolve_project(project_name) else {
                continue;
            };
            if project.clone_url.is_none() {
                continue;
            }
            out.push((project, upstream.to_string()));
        }
    }
    out
}

/// A local git worktree one of this submission's cells needs, decided (RAL-337
/// branch claim + `w/<short>` directory assignment) but not yet created --
/// see [`plan_local_worktree_jobs`]'s doc comment for why the decision and
/// the (possibly slow) git work that realizes it are split into separate
/// steps.
pub(crate) struct LocalWorktreeJob {
    cache_key: String,
    root: PathBuf,
    branch: String,
    upstream: String,
    plan: WorktreePlan,
}

/// Decide every LOCAL (no `machine` set) worktree placeholder among `cells`'
/// materialization up front -- the branch this squad gets (RAL-337) and the
/// `w/<short>` directory it lands in -- WITHOUT running the slow `git
/// worktree add`/`fetch`/rebase that used to follow immediately. Everything
/// here is fast (DB reads/writes plus local git plumbing, no network, no
/// checkout), so it's safe to run under the store lock exactly like the rest
/// of placeholder resolution always has -- only the returned jobs' actual
/// execution ([`execute_local_worktree_jobs`]) is meant to run afterward,
/// WITHOUT the lock, in parallel.
///
/// Each decided slot is immediately reserved ([`reserve_worktree_slot`]) so
/// neither a later cell in this same call nor a concurrent submission's own
/// planning pass (which may run before this call's worktrees are actually
/// created -- that's the whole point) can pick the same directory for a
/// different branch.
///
/// A cell this can't confidently handle (its project can't be resolved, its
/// `vcs` isn't `git`, its cwd isn't a direct placeholder, or it carries a
/// `machine`) is simply left out of the returned list -- the caller's later,
/// unchanged locked pass through [`resolve_placeholders_with_prefetch`]
/// resolves it exactly as it always has, live. A miss here only costs time,
/// never correctness.
#[must_use]
pub(crate) fn plan_local_worktree_jobs(
    store: &Store,
    squad_id: &str,
    cells: &[CellRow],
    tasks: &[TaskRow],
    prefetched_upstreams: &HashMap<(String, String), String>,
) -> Vec<LocalWorktreeJob> {
    let task_projects: HashMap<i64, Option<&str>> = tasks
        .iter()
        .map(|t| (t.idx, t.project.as_deref()))
        .collect();
    let mut on_disk: HashMap<PathBuf, HashMap<String, String>> = HashMap::new();
    let mut jobs: HashMap<String, LocalWorktreeJob> = HashMap::new();
    for cell in cells {
        if cell
            .machine
            .as_deref()
            .is_some_and(|m| !m.trim().is_empty())
        {
            continue;
        }
        let Some(cwd) = cell.cwd.as_deref() else {
            continue;
        };
        // Only a cwd that IS a placeholder directly -- the rarer case of one
        // embedded via `<<...>>` inside a larger string (env-override
        // expansion) isn't worth this fast path's complexity.
        let Ok(Some(branch)) = classify_placeholder(cwd) else {
            continue;
        };
        let cache_key = placeholder_cache_key(None, cwd);
        if jobs.contains_key(&cache_key) {
            continue;
        }
        let Some(project_name) = task_projects.get(&cell.task_idx).copied().flatten() else {
            continue;
        };
        let Ok(Some(project)) = store.resolve_project(project_name) else {
            continue;
        };
        if project_startup_adapter(&project.vcs).is_none() {
            continue;
        }
        let Some(raw_upstream) = ralphus_core::schema::parse_worktree_placeholder_upstream(cwd)
        else {
            continue;
        };
        let root = PathBuf::from(&project.path);
        let Ok(upstream) = resolve_upstream(&root, raw_upstream) else {
            continue;
        };
        let upstream = prefetched_upstreams
            .get(&(project.name.clone(), upstream.clone()))
            .cloned()
            .unwrap_or(upstream);
        if !on_disk.contains_key(&root) {
            on_disk.insert(root.clone(), existing_and_reserved_worktree_branches(&root));
        }
        let branch_result = {
            let snapshot = on_disk.get(&root).expect("just inserted above");
            resolve_squad_branch(store, &project, branch, squad_id, snapshot)
        };
        let Ok(branch) = branch_result else {
            continue;
        };
        let Ok(plan) = plan_worktree(&root, &branch, on_disk.get(&root).expect("populated above"))
        else {
            continue;
        };
        if !plan.already_exists {
            reserve_worktree_slot(&root, &plan, &branch);
            if let Some(short) = plan.wt.file_name().and_then(|n| n.to_str()) {
                on_disk
                    .get_mut(&root)
                    .expect("populated above")
                    .insert(short.to_string(), branch.clone());
            }
        }
        jobs.insert(
            cache_key.clone(),
            LocalWorktreeJob {
                cache_key,
                root,
                branch,
                upstream,
                plan,
            },
        );
    }
    jobs.into_values().collect()
}

/// How many [`LocalWorktreeJob`]s [`execute_local_worktree_jobs`] runs at
/// once. A ceiling on concurrent `git`/subprocess load, not tuned to any
/// particular machine -- a submission with more jobs than this just runs in
/// several back-to-back batches instead of a single one.
const MAX_PARALLEL_WORKTREE_JOBS: usize = 8;

/// Execute every planned job's (possibly slow) git work -- see
/// [`plan_local_worktree_jobs`] -- concurrently, with no store lock held.
/// Safe to run several jobs at once even against the same project root:
/// each targets a distinct, already-reserved directory and a distinct
/// branch, which is what git itself needs for concurrent `worktree add`
/// calls to be safe.
///
/// Returns resolved paths keyed exactly like [`placeholder_cache_key`], for
/// only the jobs that succeeded -- a failed job is logged and left out, so
/// the caller's later live pass (through [`resolve_placeholders_with_prefetch`])
/// retries it and surfaces the real error through the normal failure path,
/// exactly as it would if this prefetch pass had never run at all.
#[must_use]
pub(crate) fn execute_local_worktree_jobs(jobs: &[LocalWorktreeJob]) -> HashMap<String, String> {
    let mut resolved = HashMap::new();
    for chunk in jobs.chunks(MAX_PARALLEL_WORKTREE_JOBS) {
        let outcomes: Vec<(&str, Result<PathBuf, String>)> = std::thread::scope(|scope| {
            let handles: Vec<_> = chunk
                .iter()
                .map(|job| {
                    scope.spawn(move || {
                        (
                            job.cache_key.as_str(),
                            execute_worktree_plan(&job.root, &job.branch, &job.upstream, &job.plan),
                        )
                    })
                })
                .collect();
            handles.into_iter().filter_map(|h| h.join().ok()).collect()
        });
        for (key, outcome) in outcomes {
            match outcome {
                Ok(path) => {
                    resolved.insert(key.to_string(), path.to_string_lossy().into_owned());
                }
                // ralphus[ignore-rlog-pair]: this prefetch pass has no `Store` access by design (see the doc comment above); the live fallback pass records the structured failure once it retries the cell inline.
                Err(e) => crate::rlog!(
                    WARNING,
                    "ralphus [scheduler] prefetch worktree materialization failed (will retry \
                     inline): {e}"
                ),
            }
        }
    }
    resolved
}

/// Test-only entry point mirroring [`resolve_placeholders`] but taking the
/// configured machine targets directly instead of loading them from real
/// process environment variables -- tests cannot safely override
/// `RALPHUS_CONFIG_HOME`/`RALPHUS_CONFIGURATION_PATH` in-process (this
/// workspace forbids `unsafe_code`, and `std::env::set_var` requires it), so
/// this is the injection point a test that needs a specific
/// `[machine.targets.*]` entry uses instead.
#[cfg(test)]
fn resolve_placeholders_with_targets(
    store: &Store,
    squad_id: &str,
    cells: &mut [CellRow],
    tasks: &[TaskRow],
    targets: &std::collections::BTreeMap<String, crate::machine_targets::MachineTarget>,
) -> Result<(), String> {
    resolve_placeholders_inner(
        store,
        squad_id,
        cells,
        tasks,
        targets,
        &HashMap::new(),
        &HashMap::new(),
    )
    .map(|_| ())
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
    targets: &std::collections::BTreeMap<String, crate::machine_targets::MachineTarget>,
    prefetched_upstreams: &HashMap<(String, String), String>,
    prefetched_worktrees: &HashMap<String, String>,
) -> Result<usize, String> {
    let task_projects: HashMap<i64, Option<&str>> = tasks
        .iter()
        .map(|t| (t.idx, t.project.as_deref()))
        .collect();
    // Seeded from `prefetched_worktrees` (RAL-<pending>): a cell whose
    // placeholder was already materialized by an earlier, unlocked, parallel
    // pass (see `plan_local_worktree_jobs`/`execute_local_worktree_jobs`)
    // hits this cache immediately below and skips `GitProjectStartupAdapter`
    // entirely -- no store call, no git call, all under this call's lock.
    let mut cache: HashMap<String, String> = prefetched_worktrees.clone();
    // Shared across every cell below, not just within one cell's own
    // resolution -- see `PlaceholderContext::on_disk_worktrees`'s doc
    // comment for why this is safe (every worktree this pass itself creates
    // is recorded back in here immediately) and why it matters (this
    // query's cost is O(the project's total worktree count); paying it once
    // per project root per submission, rather than once per branch, is the
    // whole point).
    let on_disk_worktrees = RefCell::new(HashMap::new());
    // Prefetched entries were materialized moments ago, outside this call --
    // still real work worth reflecting in the `worktrees.materialized` span
    // attribute, even though the loop below never "discovers" them as new.
    let mut materialized = prefetched_worktrees.len();
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
            placeholder_context_for_cell(
                squad_id,
                cell,
                targets,
                prefetched_upstreams,
                &on_disk_worktrees,
            ),
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

    /// Stage every path in the worktree and commit, in-process via libgit2 --
    /// pure fixture scaffolding (PR_SLOWNESS.local.md), never the code under
    /// test, which stays on the real `git()`/`GitVcs` wrapper.
    fn git2_commit_all(
        repo: &git2::Repository,
        sig: &git2::Signature,
        message: &str,
        parents: &[&git2::Commit],
    ) -> git2::Oid {
        let mut index = repo.index().unwrap();
        index
            .add_all(["*"], git2::IndexAddOption::DEFAULT, None)
            .unwrap();
        index.write().unwrap();
        let tree = repo.find_tree(index.write_tree().unwrap()).unwrap();
        repo.commit(Some("HEAD"), sig, sig, message, &tree, parents)
            .unwrap()
    }

    fn git2_checkout(repo: &git2::Repository, branch: &str) {
        repo.set_head(&format!("refs/heads/{branch}")).unwrap();
        repo.checkout_head(Some(git2::build::CheckoutBuilder::new().force()))
            .unwrap();
    }

    /// A fresh repo with one commit on `main`.
    fn init_repo(tag: &str) -> PathBuf {
        let repo = tmp_dir(tag);
        let mut opts = git2::RepositoryInitOptions::new();
        opts.initial_head("main");
        let r = git2::Repository::init_opts(&repo, &opts).unwrap();
        std::fs::write(repo.join("base.txt"), "base\n").unwrap();
        git2_commit_all(&r, &git2::Signature::now("t", "t@t").unwrap(), "base", &[]);
        repo
    }

    fn init_repo_with_remote_branch(tag: &str, branch: &str) -> (PathBuf, String) {
        let base = tmp_dir(tag);
        let remote = base.join("remote.git");
        let (_, remote_branch) = branch
            .split_once('/')
            .expect("remote-qualified branch placeholder");

        let mut bare_opts = git2::RepositoryInitOptions::new();
        bare_opts.bare(true).initial_head("main");
        git2::Repository::init_opts(&remote, &bare_opts).unwrap();

        let seed = base.join("seed");
        let mut seed_opts = git2::RepositoryInitOptions::new();
        seed_opts.initial_head("main");
        let seed_repo = git2::Repository::init_opts(&seed, &seed_opts).unwrap();
        let sig = git2::Signature::now("t", "t@t").unwrap();

        std::fs::write(seed.join("base.txt"), "base\n").unwrap();
        let base_oid = git2_commit_all(&seed_repo, &sig, "base", &[]);
        let base_commit = seed_repo.find_commit(base_oid).unwrap();

        let mut origin = seed_repo
            .remote("origin", remote.to_str().expect("remote path"))
            .unwrap();
        origin
            .push(&["refs/heads/main:refs/heads/main"], None)
            .unwrap();

        seed_repo
            .branch(remote_branch, &base_commit, false)
            .unwrap();
        git2_checkout(&seed_repo, remote_branch);
        std::fs::write(seed.join("remote-only.txt"), format!("{branch}\n")).unwrap();
        let branch_oid = git2_commit_all(&seed_repo, &sig, "remote branch", &[&base_commit]);
        let branch_sha = branch_oid.to_string();
        origin
            .push(
                &[&format!(
                    "refs/heads/{remote_branch}:refs/heads/{remote_branch}"
                )],
                None,
            )
            .unwrap();

        let clone = base.join("clone");
        let clone_repo =
            git2::Repository::clone(remote.to_str().expect("remote path"), &clone).unwrap();
        // `ensure_worktree`'s internal resync-rebase shells out through
        // `GitVcs::exec_raw`, which (correctly, for real repos) never
        // injects an identity -- so this clone (and the worktrees it grows)
        // need one in local config, not just on this file's `g()` helper's
        // own per-invocation env vars, or a replayed commit fails identity
        // checks on a CI runner with no global gitconfig. A fresh `clone`
        // does not inherit the source repo's local config, so this can't be
        // set once upstream and skipped here.
        let mut clone_config = clone_repo.config().unwrap();
        clone_config.set_str("user.name", "t").unwrap();
        clone_config.set_str("user.email", "t@t").unwrap();
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
            maximum_context: None,
            auto_compact_threshold: None,
            maximum_tool_output_tokens: None,
            upstream: None,
            machine: None,
            share_session: false,
            maximum_timeout_sec: None,
            task_maximum_timeout_sec: None,
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

    /// A [`crate::machine_targets::MachineTarget`] for `machine`, for tests
    /// that need `resolve_placeholders_with_targets` to find one -- see that
    /// function's doc comment for why tests can't just rely on real
    /// `[machine.targets.*]` config being loaded.
    fn target_for(machine: &str, remote_root: &str) -> crate::machine_targets::MachineTarget {
        crate::machine_targets::MachineTarget {
            name: machine.replace(':', "-"),
            machine: machine.to_string(),
            remote_root: remote_root.to_string(),
            runner_mode: crate::machine_targets::RunnerMode::Installed,
            runner_command: "ralphus-runner".to_string(),
            runner_artifacts: std::collections::BTreeMap::new(),
            agent_executables: std::collections::BTreeMap::new(),
            retirement_opt_out: false,
        }
    }

    fn targets_map(
        targets: &[crate::machine_targets::MachineTarget],
    ) -> std::collections::BTreeMap<String, crate::machine_targets::MachineTarget> {
        targets
            .iter()
            .cloned()
            .map(|t| (t.name.clone(), t))
            .collect()
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
    fn ensure_worktree_forks_new_branches_from_the_resolved_upstream_not_the_checkouts_current_head()
     {
        // RAL-<pending>: a fresh task branch materialized while the shared
        // project checkout happened to have some other local branch checked
        // out must never inherit that other branch's commits just because it
        // was `HEAD` at the moment of creation.
        let repo = init_repo("head-vs-upstream");
        g(&repo, &["branch", "staging"]);
        // Simulate the shared checkout being mid-flight on `main`, with real
        // work that has nothing to do with the new task and never landed on
        // `staging`.
        std::fs::write(repo.join("unrelated.txt"), "unrelated\n").unwrap();
        g(&repo, &["add", "."]);
        g(&repo, &["commit", "--message", "unrelated work on main"]);

        let wt = ensure_worktree(&repo, "feature-new", "staging").expect("materialize");
        let log = git(&wt, &["log", "--format=%s"]).unwrap();
        assert!(
            !log.contains("unrelated work on main"),
            "new branch must fork from \"staging\" (the resolved upstream), not from whatever \
             the shared checkout's HEAD happened to be: {log}"
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
    fn ensure_worktree_mirrors_the_explicit_upstream_into_a_durable_baseline_marker() {
        // reviews.rs::worktree_baseline_ref reads `ralphus.<branch>.baseline`
        // as a push-immune fallback for the no-new-commits guard: unlike
        // `branch.<branch>.merge` (i.e. `@{upstream}`), a `finalize` cell's
        // own `git push -u` never touches this key, so it must be written
        // alongside the real upstream, not only as a UI-facing side effect.
        let repo = init_repo("new-branch-baseline");
        let main_sha = git(&repo, &["rev-parse", "main"])
            .expect("main must resolve")
            .trim()
            .to_string();
        let wt = ensure_worktree(&repo, "feature-y", "main").expect("materialize");
        let baseline = git(&wt, &["config", "--get", "ralphus.feature-y.baseline"])
            .expect("baseline marker must be recorded");
        // The marker holds the *resolved commit*, not the ref name -- a
        // later commit on `main` (or a push landing on it) must never move
        // this worktree's own baseline out from under it.
        assert_eq!(baseline.trim(), main_sha);
    }

    #[test]
    fn baseline_marker_survives_the_upstream_branch_advancing_after_materialization() {
        // Reproduces RAL-404's real false failure: something pushes new
        // commits onto the branch this worktree tracks as its upstream
        // *after* the worktree was materialized (here, a finalize cell
        // mistakenly pushing straight onto the shared base branch instead of
        // its own branch). A live ref-name baseline would silently swallow
        // that as "already accounted for"; a frozen SHA must not.
        let repo = init_repo("baseline-survives-upstream-advance");
        let wt = ensure_worktree(&repo, "feature-v", "main").expect("materialize");
        let baseline_before = git(&wt, &["config", "--get", "ralphus.feature-v.baseline"])
            .expect("baseline marker must be recorded");

        std::fs::write(wt.join("work.txt"), "work\n").unwrap();
        g(&wt, &["add", "."]);
        g(&wt, &["commit", "--message", "real work"]);

        // Something else advances the tracked upstream (`main`) past this
        // worktree's own commit -- e.g. a direct push, or another worktree
        // off the same repo committing to `main`.
        std::fs::write(repo.join("elsewhere.txt"), "unrelated\n").unwrap();
        g(&repo, &["add", "."]);
        g(&repo, &["commit", "--message", "unrelated advance of main"]);

        let baseline_after = git(&wt, &["config", "--get", "ralphus.feature-v.baseline"])
            .expect("baseline marker must still be recorded");
        assert_eq!(
            baseline_before.trim(),
            baseline_after.trim(),
            "the frozen baseline must not move just because the tracked branch advanced"
        );
        assert!(
            crate::reviews::workspace_has_commits_ahead_of_upstream(
                &crate::workspace::Workspace::local(wt.clone())
            ),
            "the worktree's own real commit must still read as progress"
        );
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
            err.contains("<<default>>"),
            "should name the sentinel: {err}"
        );
        assert!(
            err.contains("symbolic HEAD"),
            "should explain the remote default branch is unconfigured: {err}"
        );
        assert!(
            err.contains("git remote set-head origin --auto"),
            "should recommend the remediation command: {err}"
        );
        assert!(
            err.contains("literal branch name"),
            "should offer a literal upstream branch as the alternative: {err}"
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
        g(&wt, &["commit", "--message", "local agent commit"]);

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
            .register_project_with_clone_url_ex(
                "proj",
                "",
                &repo.to_string_lossy(),
                "git",
                Some(&repo.to_string_lossy()),
                None,
            )
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
        let targets = targets_map(&[target_for("ib:A", "/srv/ralphus")]);
        resolve_placeholders_with_targets(&store, "squad-1", &mut cells, &tasks, &targets)
            .expect("remote provision");
        assert_eq!(cells[0].cwd.as_deref(), Some("/remote/wt/feat-r"));
        // Nothing may be created on the daemon's own disk for a remote cell.
        assert!(
            !worktree_dir(&repo, "feat-r").exists(),
            "a remote cell must not materialize a local worktree"
        );
    }

    #[test]
    fn a_remote_cell_fails_fast_when_its_git_project_has_no_registered_clone_url() {
        // A legacy project registered with only a local `path` (no `--url`)
        // must be refused before the provider is ever dispatched, rather
        // than provisioning against a locally inferred remote.
        let repo = init_repo("remote-provision-no-url");
        let script = fake_provisioner(
            "remote-provision-no-url-p",
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
        let err = resolve_placeholders(&store, "squad-1", &mut cells, &tasks, &Context::new())
            .expect_err("a git project with no registered clone URL must fail remote provisioning");
        assert!(err.contains("has no registered clone URL"), "{err}");
        assert!(err.contains("proj"), "{err}");
    }

    #[test]
    fn a_remote_cell_fails_fast_when_its_machine_has_no_configured_target() {
        // RAL-355 Phase 2/4: even a project with a registered clone URL must
        // still be refused before provider dispatch when the target machine
        // has no `[machine.targets.*]` entry (and therefore no remote_root
        // to provision persistent storage under) -- provisioning must not
        // silently guess a location.
        let repo = init_repo("remote-provision-no-target");
        let script = fake_provisioner(
            "remote-provision-no-target-p",
            r#"{"ok":true,"protocol_version":1,"workspace":"/remote/wt/feat-r"}"#,
        );
        let store = Store::open_in_memory().unwrap();
        store
            .register_project_with_clone_url_ex(
                "proj",
                "",
                &repo.to_string_lossy(),
                "git",
                Some(&repo.to_string_lossy()),
                None,
            )
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
        // Deliberately empty -- no target configured for "ib:A".
        let targets = targets_map(&[]);
        let err =
            resolve_placeholders_with_targets(&store, "squad-1", &mut cells, &tasks, &targets)
                .expect_err("a machine with no configured target must fail remote provisioning");
        assert!(err.contains("no [machine.targets.*] entry"), "{err}");
        assert!(err.contains("ib:A"), "{err}");
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
            .register_project_with_clone_url_ex(
                "proj",
                "",
                &repo.to_string_lossy(),
                "git",
                Some(&repo.to_string_lossy()),
                None,
            )
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
        let targets = targets_map(&[
            target_for("ma:1", "/srv/ralphus-a"),
            target_for("mb:1", "/srv/ralphus-b"),
        ]);
        resolve_placeholders_with_targets(&store, "squad-1", &mut cells, &tasks, &targets)
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
            .register_project_with_clone_url_ex(
                "proj",
                "",
                &repo.to_string_lossy(),
                "git",
                Some(&repo.to_string_lossy()),
                None,
            )
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
        let targets = targets_map(&[target_for("ib:A", "/srv/ralphus")]);
        let err =
            resolve_placeholders_with_targets(&store, "squad-1", &mut cells, &tasks, &targets)
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
    fn resolve_placeholders_surfaces_a_clear_error_when_default_sentinel_has_no_remote_head() {
        // RAL-447: an unresolvable `<<default>>` (no configured remote
        // symbolic HEAD) must surface a self-service error identifying the
        // cell, the placeholder, and the repository -- not just the bare
        // low-level git failure `resolve_upstream_default_fails_...` covers
        // on its own.
        let repo = init_repo("cwd-default-no-remote");
        let store = Store::open_in_memory().unwrap();
        store
            .register_project("proj", "", &repo.to_string_lossy(), "git")
            .unwrap();
        let mut cells = vec![cell_row(
            0,
            0,
            "s0",
            Some("ralphus:new-worktree/feat?upstream=<<default>>"),
        )];
        let tasks = vec![task_row(0, Some("proj"))];
        let err = resolve_placeholders(&store, "squad-1", &mut cells, &tasks, &Context::new())
            .expect_err("no remote HEAD must be a hard error, not a guess");
        assert!(err.contains("s0"), "error should name the cell: {err}");
        assert!(
            err.contains("<<default>>"),
            "error should identify the sentinel: {err}"
        );
        assert!(
            err.contains("symbolic HEAD"),
            "error should explain the remote default branch is unconfigured: {err}"
        );
        assert!(
            err.contains("git remote set-head origin --auto"),
            "error should recommend the remediation command: {err}"
        );
        assert!(
            err.contains(&repo.display().to_string()),
            "error should name the affected repository: {err}"
        );
    }

    #[test]
    fn resolve_placeholders_honors_an_explicit_upstream_query_override() {
        // The `?upstream=` value, not the branch's own name or the repo's
        // HEAD, decides what the freshly materialized branch tracks.
        let repo = init_repo("explicit-upstream");
        g(&repo, &["checkout", "-b", "other"]);
        std::fs::write(repo.join("other.txt"), "on other\n").unwrap();
        g(&repo, &["add", "."]);
        g(&repo, &["commit", "--message", "other branch commit"]);
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
    fn resolve_placeholders_resolves_a_bare_upstream_against_the_remote_for_a_registered_project() {
        // RAL-<pending> / PR #70: for a project registered with a remote
        // (`clone_url` set), a bare `?upstream=staging` must resolve against
        // that remote's `staging` -- exactly as if the submitter had written
        // `?upstream=origin/staging` -- never against whatever the shared,
        // locally-registered checkout happens to have on disk. Without this,
        // the checkout being mid-flight on an unrelated local branch with
        // un-pushed work leaks that work into the new task branch.
        let base = tmp_dir("registered-remote-upstream");
        let remote = base.join("remote.git");
        let mut bare_opts = git2::RepositoryInitOptions::new();
        bare_opts.bare(true).initial_head("main");
        git2::Repository::init_opts(&remote, &bare_opts).unwrap();

        let seed = base.join("seed");
        let mut seed_opts = git2::RepositoryInitOptions::new();
        seed_opts.initial_head("main");
        let seed_repo = git2::Repository::init_opts(&seed, &seed_opts).unwrap();
        let sig = git2::Signature::now("t", "t@t").unwrap();
        std::fs::write(seed.join("base.txt"), "base\n").unwrap();
        let base_oid = git2_commit_all(&seed_repo, &sig, "base", &[]);
        let base_commit = seed_repo.find_commit(base_oid).unwrap();
        let mut origin = seed_repo
            .remote("origin", remote.to_str().unwrap())
            .unwrap();
        origin
            .push(&["refs/heads/main:refs/heads/main"], None)
            .unwrap();

        seed_repo.branch("staging", &base_commit, false).unwrap();
        git2_checkout(&seed_repo, "staging");
        std::fs::write(seed.join("staging-only.txt"), "staging content\n").unwrap();
        git2_commit_all(&seed_repo, &sig, "staging work", &[&base_commit]);
        origin
            .push(&["refs/heads/staging:refs/heads/staging"], None)
            .unwrap();

        let clone_path = base.join("clone");
        let clone_repo = git2::Repository::clone(remote.to_str().unwrap(), &clone_path).unwrap();
        let mut clone_config = clone_repo.config().unwrap();
        clone_config.set_str("user.name", "t").unwrap();
        clone_config.set_str("user.email", "t@t").unwrap();
        drop(clone_repo);

        // The project's shared checkout is mid-flight on an unrelated local
        // branch -- real, legitimate work, just not `staging` and never
        // pushed anywhere.
        g(&clone_path, &["checkout", "-b", "changes"]);
        std::fs::write(clone_path.join("unrelated.txt"), "local dev work\n").unwrap();
        g(&clone_path, &["add", "."]);
        g(
            &clone_path,
            &["commit", "--message", "local unrelated work"],
        );

        let store = Store::open_in_memory().unwrap();
        store
            .register_project_with_clone_url_ex(
                "proj",
                "",
                &clone_path.to_string_lossy(),
                "git",
                Some(remote.to_str().unwrap()),
                None,
            )
            .unwrap();
        let mut cells = vec![cell_row(
            0,
            0,
            "s0",
            Some("ralphus:new-worktree/feature-new?upstream=staging"),
        )];
        let tasks = vec![task_row(0, Some("proj"))];
        resolve_placeholders(&store, "squad-1", &mut cells, &tasks, &Context::new())
            .expect("materialize");
        let resolved = cells[0].cwd.clone().expect("resolved cwd");
        let log = git(Path::new(&resolved), &["log", "--format=%s"]).unwrap();
        assert!(
            log.contains("staging work"),
            "new branch must contain the remote's \"staging\" commit: {log}"
        );
        assert!(
            !log.contains("local unrelated work"),
            "new branch must not inherit the shared checkout's unrelated local branch: {log}"
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

    /// Resolve one `new-worktree` placeholder for `squad_id` and return the
    /// worktree path it materialized. RAL-337's tests all consist of doing
    /// this repeatedly under different squad ids.
    fn resolve_for_squad(store: &Store, repo: &Path, squad_id: &str, placeholder: &str) -> String {
        let _ = repo;
        let mut cells = vec![cell_row(0, 0, "s0", Some(placeholder))];
        let tasks = vec![task_row(0, Some("proj"))];
        resolve_placeholders(store, squad_id, &mut cells, &tasks, &Context::new())
            .unwrap_or_else(|e| panic!("resolve for {squad_id}: {e:?}"));
        cells[0].cwd.clone().expect("resolved cwd")
    }

    /// The branch checked out in the worktree at `wt`.
    fn head_branch(wt: &str) -> String {
        git(Path::new(wt), &["symbolic-ref", "--short", "HEAD"])
            .expect("read HEAD")
            .trim()
            .to_string()
    }

    /// The `w/<short>` directory component of a resolved worktree path.
    fn w_dir(wt: &str) -> String {
        short_name_under_w(Path::new(wt)).expect("path under .git/.ralphus/w")
    }

    #[test]
    fn ral337_a_second_squad_gets_its_own_worktree_not_the_first_squads() {
        // The RAL-337 bug in miniature: two squads submit the SAME placeholder.
        // The second must not land in the first's worktree, because the first
        // has already committed finished work onto that branch.
        let repo = init_repo("ral337-second-squad");
        let store = Store::open_in_memory().unwrap();
        store
            .register_project("proj", "", &repo.to_string_lossy(), "git")
            .unwrap();
        let ph = "ralphus:new-worktree/feat-share?upstream=main";

        let first = resolve_for_squad(&store, &repo, "squad-1", ph);
        // Squad 1 does its work and commits it -- this is what squad 2 must
        // not inherit.
        std::fs::write(Path::new(&first).join("done.txt"), "squad-1 work\n").unwrap();
        g(Path::new(&first), &["add", "done.txt"]);
        g(Path::new(&first), &["commit", "--message", "squad-1 work"]);

        let second = resolve_for_squad(&store, &repo, "squad-2", ph);

        assert_ne!(first, second, "the second squad must get its own worktree");
        assert_eq!(w_dir(&first), "feat-share");
        assert_eq!(
            w_dir(&second),
            "feat-share-2",
            "the second squad walks the existing -2 suffix sequence"
        );
        assert_eq!(head_branch(&first), "feat-share");
        assert_eq!(
            head_branch(&second),
            "feat-share-2",
            "a fresh directory is not enough -- the branch must be new too, or \
             it would check out the same commits"
        );
        assert!(
            !Path::new(&second).join("done.txt").exists(),
            "the second squad must not inherit the first squad's committed work"
        );
        // And the first squad's tree is left exactly as it was.
        assert!(Path::new(&first).join("done.txt").exists());
    }

    #[test]
    fn ral337_a_third_squad_continues_the_suffix_sequence() {
        let repo = init_repo("ral337-third-squad");
        let store = Store::open_in_memory().unwrap();
        store
            .register_project("proj", "", &repo.to_string_lossy(), "git")
            .unwrap();
        let ph = "ralphus:new-worktree/feat-seq?upstream=main";

        let a = resolve_for_squad(&store, &repo, "squad-1", ph);
        let b = resolve_for_squad(&store, &repo, "squad-2", ph);
        let c = resolve_for_squad(&store, &repo, "squad-3", ph);

        assert_eq!(
            [w_dir(&a), w_dir(&b), w_dir(&c)],
            ["feat-seq", "feat-seq-2", "feat-seq-3"]
        );
        assert_eq!(
            [head_branch(&a), head_branch(&b), head_branch(&c)],
            ["feat-seq", "feat-seq-2", "feat-seq-3"]
        );
    }

    #[test]
    fn ral337_the_owning_squad_still_reuses_its_own_worktree_across_restarts() {
        // The behavior RAL-337 must NOT break: re-resolving for the SAME squad
        // returns the same worktree and branch, with its commits intact. This
        // is what `squad restart` / `squad retry` depend on.
        let repo = init_repo("ral337-restart");
        let store = Store::open_in_memory().unwrap();
        store
            .register_project("proj", "", &repo.to_string_lossy(), "git")
            .unwrap();
        let ph = "ralphus:new-worktree/feat-restart?upstream=main";

        let first = resolve_for_squad(&store, &repo, "squad-1", ph);
        std::fs::write(Path::new(&first).join("wip.txt"), "in progress\n").unwrap();
        g(Path::new(&first), &["add", "wip.txt"]);
        g(Path::new(&first), &["commit", "--message", "wip"]);

        // A restart resolves the placeholder from scratch again, under the
        // same squad id.
        let again = resolve_for_squad(&store, &repo, "squad-1", ph);

        assert_eq!(first, again, "a restart must return to the same worktree");
        assert_eq!(head_branch(&again), "feat-restart");
        assert!(
            Path::new(&again).join("wip.txt").exists(),
            "a restart must keep the squad's own commits"
        );
    }

    #[test]
    fn ral337_one_task_may_hold_cells_in_several_different_worktrees() {
        // Per-squad allocation must NOT collapse a squad -- or a single task --
        // onto one worktree. Claims are keyed on (project, base branch, squad),
        // so two cells in the SAME task naming DIFFERENT placeholders each get
        // their own branch and their own worktree. Only cells naming the *same*
        // placeholder share one (see the test below).
        let repo = init_repo("ral337-one-task-many-worktrees");
        let store = Store::open_in_memory().unwrap();
        store
            .register_project("proj", "", &repo.to_string_lossy(), "git")
            .unwrap();
        let mut cells = vec![
            cell_row(
                0,
                0,
                "work",
                Some("ralphus:new-worktree/feat-split-a?upstream=main"),
            ),
            cell_row(
                0,
                1,
                "sidecar",
                Some("ralphus:new-worktree/feat-split-b?upstream=main"),
            ),
        ];
        let tasks = vec![task_row(0, Some("proj"))];
        resolve_placeholders(&store, "squad-1", &mut cells, &tasks, &Context::new())
            .expect("resolve both");

        let a = cells[0].cwd.clone().expect("resolved a");
        let b = cells[1].cwd.clone().expect("resolved b");
        assert_ne!(a, b, "two cells in one task must keep separate worktrees");
        assert_eq!([w_dir(&a), w_dir(&b)], ["feat-split-a", "feat-split-b"]);
        assert_eq!(
            [head_branch(&a), head_branch(&b)],
            ["feat-split-a", "feat-split-b"],
            "neither may be suffixed -- these are distinct branches, not a collision"
        );
    }

    #[test]
    fn ral337_cells_naming_the_same_placeholder_share_one_worktree() {
        // The converse of the test above: suffix allocation is per-squad, not
        // per-resolution, so two cells naming the SAME placeholder in the same
        // squad must not end up split across `feat-multi` and `feat-multi-2`.
        let repo = init_repo("ral337-one-squad-many-cells");
        let store = Store::open_in_memory().unwrap();
        store
            .register_project("proj", "", &repo.to_string_lossy(), "git")
            .unwrap();
        let ph = "ralphus:new-worktree/feat-multi?upstream=main";
        let mut cells = vec![
            cell_row(0, 0, "work", Some(ph)),
            cell_row(0, 1, "finalize", Some(ph)),
        ];
        let tasks = vec![task_row(0, Some("proj"))];
        resolve_placeholders(&store, "squad-1", &mut cells, &tasks, &Context::new())
            .expect("resolve");
        assert_eq!(cells[0].cwd, cells[1].cwd);
        assert_eq!(w_dir(cells[0].cwd.as_deref().unwrap()), "feat-multi");
    }

    #[test]
    fn ral337_a_worktree_predating_the_claims_table_is_treated_as_occupied() {
        // Migration case: a worktree materialized before RAL-337 has no claim
        // row, but is plainly still in use. A new squad must step around it
        // rather than inherit it -- otherwise the very first submission after
        // upgrading still reproduces the bug.
        let repo = init_repo("ral337-legacy-worktree");
        let store = Store::open_in_memory().unwrap();
        store
            .register_project("proj", "", &repo.to_string_lossy(), "git")
            .unwrap();
        // Materialize the way the pre-RAL-337 daemon would have: straight
        // through `ensure_worktree`, leaving no claim row behind.
        let legacy = ensure_worktree(&repo, "feat-legacy", "main").expect("legacy worktree");
        std::fs::write(legacy.join("old.txt"), "pre-upgrade work\n").unwrap();
        g(&legacy, &["add", "old.txt"]);
        g(&legacy, &["commit", "--message", "pre-upgrade work"]);

        let fresh = resolve_for_squad(
            &store,
            &repo,
            "squad-1",
            "ralphus:new-worktree/feat-legacy?upstream=main",
        );

        assert_ne!(PathBuf::from(&fresh), legacy);
        assert_eq!(head_branch(&fresh), "feat-legacy-2");
        assert!(
            !Path::new(&fresh).join("old.txt").exists(),
            "an unowned pre-existing worktree must not be inherited either"
        );
    }

    #[test]
    fn ral337_a_worktree_of_a_differently_cased_branch_is_still_treated_as_occupied() {
        // Regression: "ral-428-admin-system-prompt-tab" (a legacy worktree's
        // actual branch, left dirty by some earlier, now-unowned run) and
        // "RAL-428-admin-system-prompt-tab" (a fresh submission's placeholder
        // branch, differing only in case) are the SAME ref and the SAME
        // `w/<short>` directory on a case-insensitive filesystem (Windows,
        // default-configured macOS). A case-sensitive string comparison in
        // `resolve_squad_branch`/`resolve_task_worktree_dir_with_existing`
        // didn't know that, so a squad whose placeholder branch differed
        // only in case from an unowned leftover worktree was silently handed
        // that worktree -- and its dirty, uncommitted state -- instead of a
        // fresh "-2" slot.
        let repo = init_repo("ral337-legacy-worktree-case");
        let store = Store::open_in_memory().unwrap();
        store
            .register_project("proj", "", &repo.to_string_lossy(), "git")
            .unwrap();
        let legacy = ensure_worktree(&repo, "ral-428-admin-system-prompt-tab", "main")
            .expect("legacy worktree");
        // Left dirty (uncommitted), exactly like the real incident: an agent
        // run that started editing and never committed.
        std::fs::write(legacy.join("in-progress.txt"), "dirty leftover work\n").unwrap();

        let fresh = resolve_for_squad(
            &store,
            &repo,
            "squad-1",
            "ralphus:new-worktree/RAL-428-admin-system-prompt-tab?upstream=main",
        );

        assert_ne!(
            PathBuf::from(&fresh),
            legacy,
            "a differently-cased branch must not collide into the legacy worktree's directory"
        );
        assert_eq!(w_dir(&fresh), "RAL-428-2");
        assert_eq!(head_branch(&fresh), "RAL-428-admin-system-prompt-tab-2");
        assert!(
            !Path::new(&fresh).join("in-progress.txt").exists(),
            "the fresh squad must not inherit the legacy worktree's dirty, uncommitted state"
        );
        // And the legacy worktree is left exactly as it was.
        assert!(legacy.join("in-progress.txt").exists());
    }

    #[test]
    fn ral337_suffix_allocation_skips_slots_that_are_already_taken() {
        // With `feat-skip` and `feat-skip-2` both occupied, the next squad
        // must get `-3` -- not silently reuse either.
        let repo = init_repo("ral337-skip-taken");
        let store = Store::open_in_memory().unwrap();
        store
            .register_project("proj", "", &repo.to_string_lossy(), "git")
            .unwrap();
        let ph = "ralphus:new-worktree/feat-skip?upstream=main";
        resolve_for_squad(&store, &repo, "squad-1", ph);
        resolve_for_squad(&store, &repo, "squad-2", ph);
        let third = resolve_for_squad(&store, &repo, "squad-3", ph);
        assert_eq!(head_branch(&third), "feat-skip-3");
        assert_eq!(w_dir(&third), "feat-skip-3");
    }

    #[test]
    fn ral337_a_remote_tracking_placeholder_is_shared_across_squads_not_suffixed() {
        // A `new-worktree/origin/foo` placeholder names one specific,
        // externally owned branch. Separate squads deliberately share that
        // worktree and resync it to new pushes, so per-squad allocation must
        // NOT apply -- `origin/foo-2` would be a local branch tracking
        // nothing. Guards the behavior asserted end-to-end by
        // `worktree_projects::placeholder_cwd_origin_foo_resyncs_across_separate_run_submissions`.
        let (repo, _sha) = init_repo_with_remote_branch("ral337-remote-shared", "origin/foo");
        let store = Store::open_in_memory().unwrap();
        store
            .register_project("proj", "", &repo.to_string_lossy(), "git")
            .unwrap();
        let ph = "ralphus:new-worktree/origin/foo?upstream=origin/foo";

        let first = resolve_for_squad(&store, &repo, "squad-1", ph);
        let second = resolve_for_squad(&store, &repo, "squad-2", ph);

        assert_eq!(
            first, second,
            "a remote-tracking placeholder must resolve to the same shared worktree"
        );
        assert_eq!(head_branch(&second), "origin/foo");
    }

    #[test]
    fn ral337_suffixed_worktrees_are_still_held_to_the_path_budget() {
        // A suffix lengthens both the branch and the directory; it must not
        // become a way to slip past the MAX_PATH preflight.
        let deep = "a".repeat(120);
        assert!(
            crate::short_paths::check_worktree_path_budget(
                Path::new("/repo/.git/.ralphus/w/feat-skip-3"),
                &format!("src/{deep}.rs\0"),
                60,
            )
            .is_err(),
            "the budget check must still reject an over-long suffixed path"
        );
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

    /// RAL-<pending>: the fast-path prefetch (`plan_local_worktree_jobs` +
    /// `execute_local_worktree_jobs`) must produce results identical to the
    /// live path it's meant to short-circuit -- one worktree per distinct
    /// branch, correctly resolved and persisted -- for several cells across
    /// several tasks in one project, the exact squad-168 shape this
    /// optimization targets.
    #[test]
    fn plan_and_execute_local_worktree_jobs_materializes_every_distinct_branch() {
        let repo = init_repo("prefetch-multi-branch");
        let store = Store::open_in_memory().unwrap();
        store
            .register_project("proj", "", &repo.to_string_lossy(), "git")
            .unwrap();
        let mut cells = vec![
            cell_row(
                0,
                0,
                "s0",
                Some("ralphus:new-worktree/feat-a?upstream=main"),
            ),
            cell_row(
                1,
                0,
                "s0",
                Some("ralphus:new-worktree/feat-b?upstream=main"),
            ),
            cell_row(
                2,
                0,
                "s0",
                Some("ralphus:new-worktree/feat-c?upstream=main"),
            ),
        ];
        let tasks = vec![
            task_row(0, Some("proj")),
            task_row(1, Some("proj")),
            task_row(2, Some("proj")),
        ];

        let jobs = plan_local_worktree_jobs(&store, "squad-1", &cells, &tasks, &HashMap::new());
        assert_eq!(jobs.len(), 3, "one job per distinct branch");
        let prefetched = execute_local_worktree_jobs(&jobs);
        assert_eq!(prefetched.len(), 3, "every job must succeed");

        resolve_placeholders_with_full_prefetch(
            &store,
            "squad-1",
            &mut cells,
            &tasks,
            &HashMap::new(),
            &prefetched,
            &Context::new(),
        )
        .expect("resolution seeded entirely from the prefetch cache");

        let mut branches: Vec<String> = cells
            .iter()
            .map(|c| head_branch(c.cwd.as_deref().expect("resolved")))
            .collect();
        branches.sort();
        assert_eq!(branches, vec!["feat-a", "feat-b", "feat-c"]);

        // Restart safety must still hold: re-resolving the now-plain paths
        // (nothing left to prefetch) is a no-op that returns the same paths.
        let before: Vec<String> = cells.iter().map(|c| c.cwd.clone().unwrap()).collect();
        resolve_placeholders(&store, "squad-1", &mut cells, &tasks, &Context::new())
            .expect("restart no-op");
        let after: Vec<String> = cells.iter().map(|c| c.cwd.clone().unwrap()).collect();
        assert_eq!(before, after);
    }

    /// RAL-<pending>: the whole reason [`RESERVED_WORKTREE_SLOTS`] exists.
    /// Two branches whose short names collide (`short_name` truncates them to
    /// the same value -- see the existing `ensure_worktree_disambiguates_branches_sharing_a_short_name`
    /// test), planned by two SEPARATE calls to `plan_local_worktree_jobs`
    /// (simulating two submissions whose planning passes each take, use, and
    /// release the store lock before either one's worktree is actually
    /// created -- exactly what the deferred, parallel execution step makes
    /// possible), must still land in two DIFFERENT `w/<short>` directories,
    /// never the same one.
    #[test]
    fn plan_local_worktree_jobs_reserves_a_slot_before_it_is_ever_created() {
        let repo = init_repo("prefetch-collision");
        let store = Store::open_in_memory().unwrap();
        store
            .register_project("proj", "", &repo.to_string_lossy(), "git")
            .unwrap();

        let cells_a = vec![cell_row(
            0,
            0,
            "s0",
            Some("ralphus:new-worktree/test-pr-submission-a?upstream=main"),
        )];
        let tasks_a = vec![task_row(0, Some("proj"))];
        let jobs_a =
            plan_local_worktree_jobs(&store, "squad-1", &cells_a, &tasks_a, &HashMap::new());
        assert_eq!(jobs_a.len(), 1);

        // Squad 2's planning pass runs (and completes) BEFORE squad 1's
        // worktree has actually been created on disk -- the race this
        // registry exists to close.
        let cells_b = vec![cell_row(
            0,
            0,
            "s0",
            Some("ralphus:new-worktree/test-pr-submission-b?upstream=main"),
        )];
        let tasks_b = vec![task_row(0, Some("proj"))];
        let jobs_b =
            plan_local_worktree_jobs(&store, "squad-2", &cells_b, &tasks_b, &HashMap::new());
        assert_eq!(jobs_b.len(), 1);

        assert_eq!(
            jobs_a[0].plan.wt,
            worktree_dir(&repo, "test-pr-submission-a")
        );
        assert_ne!(
            jobs_a[0].plan.wt, jobs_b[0].plan.wt,
            "two colliding-short-name branches planned by two separate calls must not \
             be assigned the same directory"
        );

        // Both still execute cleanly into their distinct, reserved slots.
        let resolved_a = execute_local_worktree_jobs(&jobs_a);
        let resolved_b = execute_local_worktree_jobs(&jobs_b);
        assert_eq!(resolved_a.len(), 1);
        assert_eq!(resolved_b.len(), 1);
        let path_a = resolved_a.values().next().unwrap();
        let path_b = resolved_b.values().next().unwrap();
        assert_ne!(path_a, path_b);
        assert_eq!(head_branch(path_a), "test-pr-submission-a");
        assert_eq!(head_branch(path_b), "test-pr-submission-b");
    }

    fn env_ctx<'a>(
        cwd: Option<&'a str>,
        targets: &'a std::collections::BTreeMap<String, crate::machine_targets::MachineTarget>,
        prefetched_upstreams: &'a HashMap<(String, String), String>,
        on_disk_worktrees: &'a RefCell<HashMap<PathBuf, HashMap<String, String>>>,
    ) -> PlaceholderContext<'a> {
        PlaceholderContext {
            squad_id: "squad-1",
            task_idx: 0,
            cell_idx: 0,
            task_name: "task0",
            cell_id: "s0",
            machine: None,
            targets,
            prefetched_upstreams,
            on_disk_worktrees,
            cwd,
        }
    }

    #[test]
    fn materialize_env_overrides_resolves_link_to_cwd() {
        let store = Store::open_in_memory().unwrap();
        let targets = std::collections::BTreeMap::new();
        let prefetched_upstreams = HashMap::new();
        let on_disk_worktrees = RefCell::new(HashMap::new());
        let env = BTreeMap::from([("WT".to_string(), "<<ralphus:link/cwd>>".to_string())]);
        let resolved = materialize_env_overrides(
            &store,
            env_ctx(
                Some("/resolved/wt"),
                &targets,
                &prefetched_upstreams,
                &on_disk_worktrees,
            ),
            &env,
        )
        .expect("resolve link to cwd");
        assert_eq!(resolved.get("WT").map(String::as_str), Some("/resolved/wt"));
    }

    #[test]
    fn materialize_env_overrides_resolves_link_to_cwd_with_suffix() {
        let store = Store::open_in_memory().unwrap();
        let targets = std::collections::BTreeMap::new();
        let prefetched_upstreams = HashMap::new();
        let on_disk_worktrees = RefCell::new(HashMap::new());
        let env = BTreeMap::from([(
            "LOGS".to_string(),
            "<<ralphus:link/cwd?suffix=./logs>>".to_string(),
        )]);
        let resolved = materialize_env_overrides(
            &store,
            env_ctx(
                Some("/resolved/wt"),
                &targets,
                &prefetched_upstreams,
                &on_disk_worktrees,
            ),
            &env,
        )
        .expect("resolve link to cwd with suffix");
        assert_eq!(
            resolved.get("LOGS").map(String::as_str),
            Some(
                Path::new("/resolved/wt")
                    .join("./logs")
                    .to_string_lossy()
                    .as_ref()
            )
        );
    }

    #[test]
    fn materialize_env_overrides_resolves_chained_environment_link() {
        // SUB links to BASE, which itself links to cwd -- dependency-ordered
        // resolution must resolve BASE before SUB reads it, regardless of
        // BTreeMap iteration order ("BASE" < "SUB" alphabetically, so also
        // test the reverse-name case below to rule out lucky ordering).
        let store = Store::open_in_memory().unwrap();
        let targets = std::collections::BTreeMap::new();
        let prefetched_upstreams = HashMap::new();
        let on_disk_worktrees = RefCell::new(HashMap::new());
        let env = BTreeMap::from([
            ("BASE".to_string(), "<<ralphus:link/cwd>>".to_string()),
            (
                "SUB".to_string(),
                "<<ralphus:link/environment.BASE?suffix=./sub>>".to_string(),
            ),
        ]);
        let resolved = materialize_env_overrides(
            &store,
            env_ctx(
                Some("/resolved/wt"),
                &targets,
                &prefetched_upstreams,
                &on_disk_worktrees,
            ),
            &env,
        )
        .expect("resolve chained link");
        assert_eq!(
            resolved.get("BASE").map(String::as_str),
            Some("/resolved/wt")
        );
        assert_eq!(
            resolved.get("SUB").map(String::as_str),
            Some(
                Path::new("/resolved/wt")
                    .join("./sub")
                    .to_string_lossy()
                    .as_ref()
            )
        );
    }

    #[test]
    fn materialize_env_overrides_resolves_chained_link_regardless_of_map_key_order() {
        // "AFTER" sorts after "TARGET" is irrelevant here -- name the linking
        // key so it would iterate *before* its dependency alphabetically,
        // proving the resolver doesn't just get lucky with BTreeMap order.
        let store = Store::open_in_memory().unwrap();
        let targets = std::collections::BTreeMap::new();
        let prefetched_upstreams = HashMap::new();
        let on_disk_worktrees = RefCell::new(HashMap::new());
        let env = BTreeMap::from([
            (
                "AAA_LINKS_TO_ZZZ".to_string(),
                "<<ralphus:link/environment.ZZZ_BASE>>".to_string(),
            ),
            ("ZZZ_BASE".to_string(), "<<ralphus:link/cwd>>".to_string()),
        ]);
        let resolved = materialize_env_overrides(
            &store,
            env_ctx(
                Some("/resolved/wt"),
                &targets,
                &prefetched_upstreams,
                &on_disk_worktrees,
            ),
            &env,
        )
        .expect("resolve chained link");
        assert_eq!(
            resolved.get("AAA_LINKS_TO_ZZZ").map(String::as_str),
            Some("/resolved/wt")
        );
    }

    #[test]
    fn materialize_env_overrides_link_to_a_plain_literal_field_resolves_directly() {
        // The linked-to field (`cwd`) is a plain literal here, never a
        // worktree placeholder -- no project is registered and none is
        // needed, since resolving a link never triggers worktree
        // materialization.
        let store = Store::open_in_memory().unwrap();
        let targets = std::collections::BTreeMap::new();
        let prefetched_upstreams = HashMap::new();
        let on_disk_worktrees = RefCell::new(HashMap::new());
        let env = BTreeMap::from([("WT".to_string(), "<<ralphus:link/cwd>>".to_string())]);
        let resolved = materialize_env_overrides(
            &store,
            env_ctx(
                Some("/plain/checkout"),
                &targets,
                &prefetched_upstreams,
                &on_disk_worktrees,
            ),
            &env,
        )
        .expect("resolve link to a plain literal");
        assert_eq!(
            resolved.get("WT").map(String::as_str),
            Some("/plain/checkout")
        );
    }

    #[test]
    fn materialize_env_overrides_leaves_non_link_values_untouched() {
        // Regression guard (RAL-100/RAL-447): a plain literal or an embedded
        // worktree placeholder must resolve exactly as it did before linked
        // fields existed.
        let store = Store::open_in_memory().unwrap();
        let targets = std::collections::BTreeMap::new();
        let prefetched_upstreams = HashMap::new();
        let on_disk_worktrees = RefCell::new(HashMap::new());
        let env = BTreeMap::from([("PLAIN".to_string(), "just-a-literal".to_string())]);
        let resolved = materialize_env_overrides(
            &store,
            env_ctx(None, &targets, &prefetched_upstreams, &on_disk_worktrees),
            &env,
        )
        .expect("resolve plain literal");
        assert_eq!(
            resolved.get("PLAIN").map(String::as_str),
            Some("just-a-literal")
        );
    }

    #[test]
    fn materialize_env_overrides_errors_on_a_two_key_link_cycle() {
        let store = Store::open_in_memory().unwrap();
        let targets = std::collections::BTreeMap::new();
        let prefetched_upstreams = HashMap::new();
        let on_disk_worktrees = RefCell::new(HashMap::new());
        let env = BTreeMap::from([
            (
                "A".to_string(),
                "<<ralphus:link/environment.B>>".to_string(),
            ),
            (
                "B".to_string(),
                "<<ralphus:link/environment.A>>".to_string(),
            ),
        ]);
        let err = materialize_env_overrides(
            &store,
            env_ctx(
                Some("/wt"),
                &targets,
                &prefetched_upstreams,
                &on_disk_worktrees,
            ),
            &env,
        )
        .expect_err("a circular link chain must fail resolution");
        assert!(err.contains("circular"), "{err}");
    }

    #[test]
    fn materialize_env_overrides_errors_when_cwd_link_has_no_cwd_in_scope() {
        let store = Store::open_in_memory().unwrap();
        let targets = std::collections::BTreeMap::new();
        let prefetched_upstreams = HashMap::new();
        let on_disk_worktrees = RefCell::new(HashMap::new());
        let env = BTreeMap::from([("WT".to_string(), "<<ralphus:link/cwd>>".to_string())]);
        let err = materialize_env_overrides(
            &store,
            env_ctx(None, &targets, &prefetched_upstreams, &on_disk_worktrees),
            &env,
        )
        .expect_err("linking to cwd with no cwd in scope must fail");
        assert!(err.contains("cwd"), "{err}");
    }
}
