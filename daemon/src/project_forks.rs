//! Per-project, per-user fork registration (RAL-338).
//!
//! A **fork** is the writable repository review branches are pushed to when
//! the acting user cannot push directly to a project's registered **parent**
//! repository. Rows are keyed by `(project, user)`, with `user == ""` acting
//! as the project-wide fallback row used when no user-specific row exists
//! (see [`Store::resolve_fork`]).
//!
//! The `user` column is a lookup detail, not an authorization boundary: any
//! caller able to reach the daemon's API can create, edit, list, or remove
//! any fork row, including one naming a user who was later deleted from the
//! `users` table. Rows deliberately do not cascade on user deletion -- see
//! this module's schema comment in `store.rs` -- so a fork registered for a
//! since-removed user stays visible (health checks and the board must flag
//! it, not hide it).
//!
//! Ralphus only *registers* existing forks; it never creates one through a
//! forge API (see the ticket's Out of Scope).

use rusqlite::OptionalExtension as _;
use serde::Serialize;

use crate::store::{Result as StoreResult, Store, StoreError, now_ms};

/// One registered fork row.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ForkRecord {
    pub project: String,
    /// `""` for the project-wide default row.
    pub user: String,
    pub fork_url: String,
    pub remote_name: String,
    /// GitHub owner/org login the fork lives under, used to build the
    /// `owner:branch` cross-repo PR head. Left `""` for GitLab, which
    /// addresses cross-project MRs by numeric project id instead.
    pub fork_owner: String,
    /// Git `user.name` to apply to a worktree owned by this fork's resolved
    /// user (via `git config --worktree`). `None` = don't override --
    /// inherit whatever the worktree's/checkout's own git config already
    /// resolves to.
    pub git_user_name: Option<String>,
    /// Git `user.email`, same override semantics as `git_user_name`.
    pub git_user_email: Option<String>,
    pub created_at_ms: i64,
    pub updated_at_ms: i64,
}

const FORK_COLUMNS: &str = "project, user, fork_url, remote_name, fork_owner, git_user_name, \
                             git_user_email, created_at_ms, updated_at_ms";

fn row_to_fork_record(r: &rusqlite::Row<'_>) -> rusqlite::Result<ForkRecord> {
    Ok(ForkRecord {
        project: r.get(0)?,
        user: r.get(1)?,
        fork_url: r.get(2)?,
        remote_name: r.get(3)?,
        fork_owner: r.get(4)?,
        git_user_name: r.get(5)?,
        git_user_email: r.get(6)?,
        created_at_ms: r.get(7)?,
        updated_at_ms: r.get(8)?,
    })
}

/// The git identity to apply to a fork-owned worktree (RAL-338 follow-up):
/// resolved from a [`ForkRecord`]'s `git_user_name`/`git_user_email`, kept
/// as its own type so a caller with no fork resolved (or a fork that sets
/// neither field) can be handed a plain `None` instead of an empty record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct GitIdentity {
    pub name: Option<String>,
    pub email: Option<String>,
}

/// Default remote name for a fork row: `"fork"` for the project-wide default
/// (`user == ""`), else `"fork-<sanitized-user>"`.
#[must_use]
pub fn default_remote_name(user: &str) -> String {
    if user.is_empty() {
        "fork".to_string()
    } else {
        format!("fork-{}", sanitize_remote_component(user))
    }
}

/// Git remote names only tolerate a limited character set; anything else in
/// a user name becomes `-` so `default_remote_name` always produces a valid
/// `git remote add` name.
fn sanitize_remote_component(s: &str) -> String {
    s.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.' {
                c
            } else {
                '-'
            }
        })
        .collect()
}

impl Store {
    /// Idempotent upsert of a fork record for `(project, user)`. Preserves
    /// `created_at_ms` across repeat calls; always refreshes `updated_at_ms`.
    ///
    /// # Errors
    /// Propagates any SQLite failure.
    pub fn upsert_project_fork(
        &self,
        project: &str,
        user: &str,
        fork_url: &str,
        remote_name: &str,
        fork_owner: &str,
    ) -> StoreResult<ForkRecord> {
        self.upsert_project_fork_with_identity(
            project,
            user,
            fork_url,
            remote_name,
            fork_owner,
            None,
            None,
        )
    }

    /// Same as [`Self::upsert_project_fork`], additionally setting the
    /// fork's git identity override (`None` for either leaves that field
    /// untouched by a re-upsert, i.e. preserves whatever was there before --
    /// see `patch_project_fork` for a purely field-selective alternative
    /// when only the identity should change).
    ///
    /// # Errors
    /// Propagates any SQLite failure.
    #[allow(clippy::too_many_arguments)]
    pub fn upsert_project_fork_with_identity(
        &self,
        project: &str,
        user: &str,
        fork_url: &str,
        remote_name: &str,
        fork_owner: &str,
        git_user_name: Option<&str>,
        git_user_email: Option<&str>,
    ) -> StoreResult<ForkRecord> {
        let now = now_ms();
        self.conn.execute(
            "INSERT INTO project_forks(project, user, fork_url, remote_name, fork_owner, git_user_name, git_user_email, created_at_ms, updated_at_ms)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?8)
             ON CONFLICT(project, user) DO UPDATE SET
                fork_url = excluded.fork_url,
                remote_name = excluded.remote_name,
                fork_owner = excluded.fork_owner,
                git_user_name = COALESCE(excluded.git_user_name, project_forks.git_user_name),
                git_user_email = COALESCE(excluded.git_user_email, project_forks.git_user_email),
                updated_at_ms = excluded.updated_at_ms",
            rusqlite::params![project, user, fork_url, remote_name, fork_owner, git_user_name, git_user_email, now],
        )?;
        crate::rlog!(
            INFO,
            "ralphus [store] fork registered for project {project:?} user {user:?} -> {fork_url:?}"
        );
        let _ = self.cartographer_log(crate::cartographer::CartographerEntry {
            level: crate::logging::LogLevel::INFO,
            source: "store",
            message: "project fork registered",
            scope: Some("project_fork"),
            squad_id: None,
            guardian_id: None,
            cell_id: None,
            task: None,
            log_path: None,
            payload: serde_json::json!({ "project": project, "user": user, "fork_url": fork_url }),
            admin_only: false,
        });
        self.get_project_fork(project, user)?
            .ok_or(StoreError::NotFound)
    }

    /// Field-selective patch -- only `Some(..)` fields change. Returns
    /// [`StoreError::NotFound`] if no row exists for `(project, user)`.
    ///
    /// # Errors
    /// Propagates any SQLite failure.
    pub fn patch_project_fork(
        &self,
        project: &str,
        user: &str,
        fork_url: Option<&str>,
        remote_name: Option<&str>,
        fork_owner: Option<&str>,
    ) -> StoreResult<ForkRecord> {
        self.patch_project_fork_ex(project, user, fork_url, remote_name, fork_owner, None, None)
    }

    /// Same as [`Self::patch_project_fork`], additionally field-selective
    /// over the git identity override (`git_user_name`/`git_user_email`).
    ///
    /// # Errors
    /// Propagates any SQLite failure, or [`StoreError::NotFound`] if no row
    /// exists for `(project, user)`.
    #[allow(clippy::too_many_arguments)]
    pub fn patch_project_fork_ex(
        &self,
        project: &str,
        user: &str,
        fork_url: Option<&str>,
        remote_name: Option<&str>,
        fork_owner: Option<&str>,
        git_user_name: Option<&str>,
        git_user_email: Option<&str>,
    ) -> StoreResult<ForkRecord> {
        let Some(existing) = self.get_project_fork(project, user)? else {
            return Err(StoreError::NotFound);
        };
        let fork_url = fork_url.unwrap_or(&existing.fork_url);
        let remote_name = remote_name.unwrap_or(&existing.remote_name);
        let fork_owner = fork_owner.unwrap_or(&existing.fork_owner);
        let git_user_name = git_user_name.or(existing.git_user_name.as_deref());
        let git_user_email = git_user_email.or(existing.git_user_email.as_deref());
        let now = now_ms();
        self.conn.execute(
            "UPDATE project_forks SET fork_url=?1, remote_name=?2, fork_owner=?3, git_user_name=?4, git_user_email=?5, updated_at_ms=?6
             WHERE project=?7 AND user=?8",
            rusqlite::params![fork_url, remote_name, fork_owner, git_user_name, git_user_email, now, project, user],
        )?;
        crate::rlog!(
            INFO,
            "ralphus [store] fork for project {project:?} user {user:?} updated"
        );
        let _ = self.cartographer_log(crate::cartographer::CartographerEntry {
            level: crate::logging::LogLevel::INFO,
            source: "store",
            message: "project fork updated",
            scope: Some("project_fork"),
            squad_id: None,
            guardian_id: None,
            cell_id: None,
            task: None,
            log_path: None,
            payload: serde_json::json!({ "project": project, "user": user }),
            admin_only: false,
        });
        self.get_project_fork(project, user)?
            .ok_or(StoreError::NotFound)
    }

    /// A fork row by its exact `(project, user)` key, or `None`.
    ///
    /// # Errors
    /// Propagates any SQLite failure.
    pub fn get_project_fork(&self, project: &str, user: &str) -> StoreResult<Option<ForkRecord>> {
        self.conn
            .query_row(
                &format!("SELECT {FORK_COLUMNS} FROM project_forks WHERE project=?1 AND user=?2"),
                rusqlite::params![project, user],
                row_to_fork_record,
            )
            .optional()
            .map_err(Into::into)
    }

    /// Every registered fork row, across every project and user.
    ///
    /// # Errors
    /// Propagates any SQLite failure.
    pub fn list_project_forks(&self) -> StoreResult<Vec<ForkRecord>> {
        let mut stmt = self.conn.prepare(&format!(
            "SELECT {FORK_COLUMNS} FROM project_forks ORDER BY project, user"
        ))?;
        let rows = stmt
            .query_map([], row_to_fork_record)?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// Every fork row registered for one project (including its `user=""`
    /// default row, if any).
    ///
    /// # Errors
    /// Propagates any SQLite failure.
    pub fn list_project_forks_for_project(&self, project: &str) -> StoreResult<Vec<ForkRecord>> {
        let mut stmt = self.conn.prepare(&format!(
            "SELECT {FORK_COLUMNS} FROM project_forks WHERE project=?1 ORDER BY user"
        ))?;
        let rows = stmt
            .query_map(rusqlite::params![project], row_to_fork_record)?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// Every fork row registered for one user, across projects.
    ///
    /// # Errors
    /// Propagates any SQLite failure.
    pub fn list_project_forks_for_user(&self, user: &str) -> StoreResult<Vec<ForkRecord>> {
        let mut stmt = self.conn.prepare(&format!(
            "SELECT {FORK_COLUMNS} FROM project_forks WHERE user=?1 ORDER BY project"
        ))?;
        let rows = stmt
            .query_map(rusqlite::params![user], row_to_fork_record)?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// Resolve the exact fork row for `(project, user)`. A submitting user
    /// with no personal row receives no fork and continues through origin;
    /// the empty user is reserved for daemon-internal/default routing.
    ///
    /// # Errors
    /// Propagates any SQLite failure.
    pub fn resolve_fork(&self, project: &str, user: &str) -> StoreResult<Option<ForkRecord>> {
        self.get_project_fork(project, user)
    }

    /// Remove a fork record for `(project, user)`. Returns `false` if none
    /// existed.
    ///
    /// # Errors
    /// Propagates any SQLite failure.
    pub fn delete_project_fork(&self, project: &str, user: &str) -> StoreResult<bool> {
        let n = self.conn.execute(
            "DELETE FROM project_forks WHERE project=?1 AND user=?2",
            rusqlite::params![project, user],
        )?;
        if n > 0 {
            crate::rlog!(
                INFO,
                "ralphus [store] fork for project {project:?} user {user:?} removed"
            );
            let _ = self.cartographer_log(crate::cartographer::CartographerEntry {
                level: crate::logging::LogLevel::INFO,
                source: "store",
                message: "project fork removed",
                scope: Some("project_fork"),
                squad_id: None,
                guardian_id: None,
                cell_id: None,
                task: None,
                log_path: None,
                payload: serde_json::json!({ "project": project, "user": user }),
                admin_only: false,
            });
        }
        Ok(n > 0)
    }
}

/// Best-effort resolution of the forge host a project's own remote points
/// at (RAL-500), used to reject a fork registration whose URL targets a
/// different forge host than the project it's being registered against.
/// Prefers the project's explicitly registered `clone_url` (RAL-355) over
/// reading the local git remote, since a project can be registered without
/// ever having been cloned onto this daemon host. Falls back to the local
/// git remote picked by [`crate::forge::default_remote_name`] (the same
/// branch-independent fallback project provisioning itself uses, since a
/// fork-registration call has no review `base_branch` to resolve a remote
/// name from). Returns `None` when neither source is set/parseable --
/// callers should treat "no determinable host" as "skip the host-match
/// check" rather than a rejection, since plenty of registered projects have
/// no clone URL and no local checkout on this machine.
#[must_use]
pub(crate) fn resolve_project_forge_host(project: &crate::store::ProjectView) -> Option<String> {
    if let Some(clone_url) = project
        .clone_url
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        if let Some((host, _)) = crate::forge::parse_remote_url(clone_url) {
            return Some(host);
        }
    }
    let root = std::path::Path::new(&project.path);
    let forge_cfg = crate::config::resolve_forge(root);
    let remote_name = crate::forge::default_remote_name(&forge_cfg);
    let url = crate::guardian_merge::git(
        root,
        &["config", "--get", &format!("remote.{remote_name}.url")],
    )
    .ok()?;
    crate::forge::parse_remote_url(url.trim()).map(|(host, _)| host)
}

/// Idempotently add or update a local git remote pointing at `fork_url`
/// under `remote_name`, in the working tree rooted at `root`. Safe to call
/// before every fork-mode push -- a no-op when the remote already points at
/// `fork_url`.
///
/// Compares against `git config --get remote.<name>.url` rather than `git
/// remote get-url`: the latter applies any global `url.<x>.insteadOf`
/// rewrite rule (e.g. rewriting `https://github.com/` to an SSH form for the
/// caller's own auth preference), so a caller with such a rule configured
/// would otherwise see every already-correct fork remote read back rewritten
/// and be "corrected" back and forth every call. `git config --get` reads
/// the literal configured value with no such rewriting.
///
/// # Errors
/// Returns the underlying git error string on failure.
pub(crate) fn ensure_fork_remote(
    root: &std::path::Path,
    remote_name: &str,
    fork_url: &str,
) -> Result<(), String> {
    match crate::guardian_merge::git(
        root,
        &["config", "--get", &format!("remote.{remote_name}.url")],
    ) {
        Ok(existing) if existing.trim() == fork_url => Ok(()),
        Ok(_) => crate::guardian_merge::git(root, &["remote", "set-url", remote_name, fork_url])
            .map(|_| ()),
        Err(_) => {
            crate::guardian_merge::git(root, &["remote", "add", remote_name, fork_url]).map(|_| ())
        }
    }
}

/// The unsuffixed name of a review's transient fork-side upstream branch
/// (RAL-<new>): `ralphus/review/<guardian-id>/upstream`. This branch -- never
/// the fork's own real base branch -- is what a `dual_root_pr` stack PR
/// targets, and what is force-pushed to the parent's current base tip every
/// time ralphus fetches that base for the review. It is a git ref, not a
/// local path, so its full descriptive name is acceptable; any disk artifact
/// derived from it must use a shorter name (Windows `MAX_PATH`).
#[must_use]
pub(crate) fn review_upstream_branch_base(guardian_id: &str) -> String {
    format!("ralphus/review/{guardian_id}/upstream")
}

/// Pick a unique transient fork-side upstream branch name for one review
/// (RAL-<new>): [`review_upstream_branch_base`], collision-suffixed `-2`,
/// `-3`, ... against the heads that already exist on the fork remote, using
/// the same walk as `review_branch::resolve_unique`. The result is persisted
/// (`guardians.dual_root_stack_branch`) by the caller and never recomputed,
/// so later syncs and promotions keep targeting the same ref.
///
/// The collision walk matters even though the base name already embeds the
/// guardian id: a fork may carry leftovers from a review that was deleted
/// before its branch could be retired, and reusing a dead review's ref would
/// force-push over whatever still points at it.
///
/// # Errors
/// Propagates the underlying `git ls-remote` failure.
pub(crate) fn allocate_review_upstream_branch(
    root: &std::path::Path,
    fork_remote_name: &str,
    guardian_id: &str,
) -> std::result::Result<String, String> {
    let base = review_upstream_branch_base(guardian_id);
    let listing = crate::guardian_merge::git(
        root,
        &[
            "ls-remote",
            "--heads",
            fork_remote_name,
            &format!("{base}*"),
        ],
    )
    .map_err(|e| format!("could not list fork branches for the dual-root upstream ref: {e}"))?;
    let taken: std::collections::HashSet<String> = listing
        .lines()
        .filter_map(|line| line.split_once('\t'))
        .map(|(_, head)| head.trim().trim_start_matches("refs/heads/").to_string())
        .collect();
    crate::review_branch::resolve_unique(&base, |name| taken.contains(name))
        .ok_or_else(|| format!("could not find a free dual-root upstream branch for {guardian_id}"))
}

/// Force-push the parent branch's current tip onto a review's transient
/// fork-side upstream branch (RAL-<new>): the ref a `dual_root_pr` stack PR
/// targets, kept exactly mirroring the parent's base so the stack PR always
/// sits on the review's real intended upstream. `root` must already carry
/// both remotes, with the fork remote's credential helper/identity already
/// wired (see `pr::resolve_fork_routing`, which every caller of this function
/// runs through first).
///
/// The push is deliberately `--force`, unlike the fork's own base branch
/// (which ralphus never touches): this ref is disposable and owned entirely
/// by the review, so overwriting it can never lose anyone else's work.
/// Returns the pushed tip sha when a push was needed, `None` when the branch
/// was already current -- an already-current branch skips the push entirely,
/// so periodic refreshes stay cheap. RAL-510: the caller uses the returned
/// sha as `cancel_superseded_ci`'s `keep_sha` for this force-push.
///
/// # Errors
/// Propagates the underlying `git fetch`/`git push` failure.
pub(crate) fn sync_review_upstream_branch(
    root: &std::path::Path,
    parent_remote_name: &str,
    fork_remote_name: &str,
    base_branch_name: &str,
    review_branch: &str,
) -> std::result::Result<Option<String>, String> {
    crate::guardian_merge::git(root, &["fetch", parent_remote_name, base_branch_name])
        .map_err(|e| format!("could not fetch {parent_remote_name}/{base_branch_name}: {e}"))?;
    let tip = crate::guardian_merge::git(root, &["rev-parse", "FETCH_HEAD"])
        .map(|s| s.trim().to_string())
        .map_err(|e| format!("could not resolve fetched tip: {e}"))?;
    let current = crate::guardian_merge::git(
        root,
        &[
            "ls-remote",
            fork_remote_name,
            &format!("refs/heads/{review_branch}"),
        ],
    )
    .map_err(|e| format!("could not read the fork's {review_branch} ref: {e}"))?
    .lines()
    .next()
    .and_then(|line| line.split_once('\t'))
    .map(|(sha, _)| sha.trim().to_string());
    if current.as_deref() == Some(tip.as_str()) {
        return Ok(None);
    }
    crate::guardian_merge::git(
        root,
        &[
            "push",
            "--force",
            fork_remote_name,
            &format!("{tip}:refs/heads/{review_branch}"),
        ],
    )
    .map(|_| Some(tip))
    .map_err(|e| format!("could not force-push the review upstream branch {review_branch}: {e}"))
}

/// Delete a review's transient fork-side upstream branch (RAL-<new>): the
/// teardown half of the branch's lifecycle, run when the review reaches a
/// terminal state or is deleted. A ref the fork remote no longer has is a
/// successful no-op (it may already have been retired by an earlier pass).
///
/// # Errors
/// Propagates the underlying `git push --delete` failure; callers treat
/// retirement as best-effort and retry on a later sweep.
pub(crate) fn delete_review_upstream_branch(
    root: &std::path::Path,
    fork_remote_name: &str,
    review_branch: &str,
) -> std::result::Result<(), String> {
    let exists = crate::guardian_merge::git(
        root,
        &[
            "ls-remote",
            "--heads",
            fork_remote_name,
            &format!("refs/heads/{review_branch}"),
        ],
    )
    .map_err(|e| format!("could not read the fork's {review_branch} ref: {e}"))?;
    if exists.trim().is_empty() {
        return Ok(());
    }
    crate::guardian_merge::git(root, &["push", fork_remote_name, "--delete", review_branch])
        .map(|_| ())
        .map_err(|e| format!("could not delete the review upstream branch {review_branch}: {e}"))
}

/// One health finding for a registered fork row (RAL-338 Phase 6). Advisory
/// only -- see [`check_fork_health`]'s doc comment; submission's own
/// pre-flight (`crate::pr::run_fork_preflight`) is what actually prevents a
/// broken fork from being used, using the exact same relationship
/// classification this reuses rather than a second implementation.
#[derive(Debug, Clone, Serialize)]
pub struct ForkHealthCheck {
    pub project: String,
    /// `""` for the project-wide default row.
    pub user: String,
    pub name: &'static str,
    pub status: &'static str,
    pub detail: String,
}

fn check(
    project: &str,
    user: &str,
    name: &'static str,
    status: &'static str,
    detail: impl Into<String>,
) -> ForkHealthCheck {
    ForkHealthCheck {
        project: project.to_string(),
        user: user.to_string(),
        name,
        status,
        detail: detail.into(),
    }
}

/// Health checks for one registered fork row (RAL-338): missing local git
/// remote, unregistered user, and forge relationship/reachability/instance
/// problems -- reusing [`crate::forge::classify_fork_relationship`] rather
/// than a second relationship walk, per this ticket's explicit requirement.
/// Advisory only: a `fail` here does not block anything by itself (unlike
/// the submission-time pre-flight, which is authoritative) -- it exists so
/// an operator can discover a broken fork registration before a review ever
/// tries to submit through it.
#[must_use]
pub fn check_fork_health(store: &Store, fork: &ForkRecord) -> Vec<ForkHealthCheck> {
    let (mut out, project_path) = fork_health_store_inputs(store, fork);
    if let Some(project_path) = project_path {
        out.extend(check_fork_network_health(&project_path, fork));
    }
    out
}

/// The store-read prologue of [`check_fork_health`]: the orphaned-user and
/// missing-project conditions (both pure store reads), plus the registered
/// project's path for the caller to hand to
/// [`check_fork_network_health`] *without holding the store lock*. A `None`
/// path means the network probe must be skipped -- a fail-class
/// `fork-project` check is already in the returned list.
#[must_use]
pub fn fork_health_store_inputs(
    store: &Store,
    fork: &ForkRecord,
) -> (Vec<ForkHealthCheck>, Option<std::path::PathBuf>) {
    let mut out = Vec::new();
    if !fork.user.is_empty() && matches!(store.get_user(&fork.user), Ok(None)) {
        out.push(check(
            &fork.project,
            &fork.user,
            "fork-user",
            "warn",
            format!(
                "fork registered for user {:?}, who is no longer registered -- the row                  still applies to that name if resolved (RAL-338: rows deliberately                  survive user deletion), but the board should flag it as orphaned",
                fork.user
            ),
        ));
    }
    let project = match store.get_project(&fork.project) {
        Ok(Some(p)) => p,
        Ok(None) => {
            out.push(check(
                &fork.project,
                &fork.user,
                "fork-project",
                "fail",
                "the project this fork was registered against is no longer registered",
            ));
            return (out, None);
        }
        Err(e) => {
            out.push(check(
                &fork.project,
                &fork.user,
                "fork-project",
                "fail",
                e.to_string(),
            ));
            return (out, None);
        }
    };
    (out, Some(std::path::PathBuf::from(project.path)))
}

/// The I/O tail of [`check_fork_health`]: a `git config --get` subprocess
/// plus the forge-relationship REST probes. Takes no store access, so it can
/// run with no store guard held.
pub fn check_fork_network_health(
    project_path: &std::path::Path,
    fork: &ForkRecord,
) -> Vec<ForkHealthCheck> {
    let mut out = Vec::new();
    let root = project_path;
    // `git config --get` (not `git remote get-url`) bypasses any
    // `url.<x>.insteadOf` rewrite rule in the caller's git config (e.g.
    // rewriting `https://github.com/` to an SSH form) so this compares
    // against the literal URL that was registered, not a locally-rewritten
    // form that would otherwise read as a false mismatch.
    match crate::guardian_merge::git(
        root,
        &[
            "config",
            "--get",
            &format!("remote.{}.url", fork.remote_name),
        ],
    ) {
        Ok(url) if url.trim() == fork.fork_url => {}
        Ok(url) => out.push(check(
            &fork.project,
            &fork.user,
            "fork-remote",
            "warn",
            format!(
                "local remote {:?} in {:?} points at {:?}, not the registered fork_url {:?} --                  the next submission through this fork will correct it automatically",
                fork.remote_name,
                project_path.display(),
                url.trim(),
                fork.fork_url
            ),
        )),
        Err(_) => out.push(check(
            &fork.project,
            &fork.user,
            "fork-remote",
            "warn",
            format!(
                "no local git remote named {:?} configured in {:?} yet -- it will be created                  automatically the next time this fork is used to submit",
                fork.remote_name, project_path.display()
            ),
        )),
    }

    let forge_cfg = crate::config::resolve_forge(root);
    let parent_remote_name = crate::forge::resolve_remote_name_excluding(
        root,
        "", // no review base-branch context at health-check time
        &forge_cfg,
        Some(&fork.remote_name),
    );
    let parent_client = crate::forge::resolve_remote_for(root, &parent_remote_name, &forge_cfg);
    let fork_client = crate::forge::resolve_remote_for(root, &fork.remote_name, &forge_cfg);
    match (parent_client, fork_client) {
        (Ok(parent_client), Ok(fork_client)) => {
            let fork_lookup = fork_client.lookup_fork_network();
            let parent_lookup = parent_client.lookup_fork_network();
            let relationship = crate::forge::classify_fork_relationship(
                fork_client.kind(),
                parent_client.kind(),
                &fork_lookup,
                &parent_lookup,
            );
            let (status, detail): (&'static str, &'static str) = match relationship {
                crate::forge::ForkRelationship::SameNetwork => (
                    "pass",
                    "fork is a direct, forge-confirmed fork of the parent",
                ),
                crate::forge::ForkRelationship::SameNetworkIndirect => (
                    "warn",
                    "fork is only indirectly related to the parent (a fork of a fork, or a                      sibling) -- cross-repository behavior is only proven for a direct fork",
                ),
                crate::forge::ForkRelationship::NoRelationship => (
                    "fail",
                    "fork does not appear to be forge-related to the parent at all --                      submission through it will be blocked without --allow-unlinked-fork",
                ),
                crate::forge::ForkRelationship::NotVisible => (
                    "warn",
                    "could not confirm the fork relationship over the forge API (private,                      deleted, or missing token) -- this is not proof of no relationship",
                ),
                crate::forge::ForkRelationship::CrossInstance => (
                    "fail",
                    "the fork and its parent are on different forge instances/kinds --                      there is no cross-repository PR path between them",
                ),
            };
            out.push(check(
                &fork.project,
                &fork.user,
                "fork-relationship",
                status,
                detail,
            ));
        }
        (Err(e), _) | (_, Err(e)) => out.push(check(
            &fork.project,
            &fork.user,
            "fork-relationship",
            "fail",
            format!("could not resolve a forge client to check this fork: {e}"),
        )),
    }
    out
}

#[cfg(test)]
mod health_tests {
    use super::*;

    fn tmp_dir(tag: &str) -> std::path::PathBuf {
        static N: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let n = N.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "ralphus-fork-health-test-{tag}-{}-{n}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn flags_an_unregistered_user_and_a_missing_local_remote() {
        let root = tmp_dir("basic");
        crate::guardian_merge::git(&root, &["init"]).unwrap();

        let store = Store::open_in_memory().unwrap();
        store
            .register_project("demo", "d", root.to_str().unwrap(), "git")
            .unwrap();
        let fork = store
            .upsert_project_fork(
                "demo",
                "ghost",
                "https://github.com/alice/widget.git",
                "fork",
                "alice",
            )
            .unwrap();

        let checks = check_fork_health(&store, &fork);
        let user_check = checks.iter().find(|c| c.name == "fork-user").unwrap();
        assert_eq!(user_check.status, "warn");
        assert!(user_check.detail.contains("ghost"));
        let remote_check = checks.iter().find(|c| c.name == "fork-remote").unwrap();
        assert_eq!(remote_check.status, "warn");
        assert!(remote_check.detail.contains("fork"));

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn reports_a_missing_project_as_a_failure() {
        let store = Store::open_in_memory().unwrap();
        let fork = store
            .upsert_project_fork(
                "ghost-project",
                "",
                "https://github.com/alice/widget.git",
                "fork",
                "alice",
            )
            .unwrap();
        let checks = check_fork_health(&store, &fork);
        assert_eq!(checks.len(), 1);
        assert_eq!(checks[0].name, "fork-project");
        assert_eq!(checks[0].status, "fail");
    }

    #[test]
    fn passes_the_local_remote_check_once_the_url_matches() {
        let root = tmp_dir("remote-ok");
        crate::guardian_merge::git(&root, &["init"]).unwrap();
        crate::guardian_merge::git(
            &root,
            &[
                "remote",
                "add",
                "fork",
                "https://github.com/alice/widget.git",
            ],
        )
        .unwrap();

        let store = Store::open_in_memory().unwrap();
        store
            .register_project("demo", "d", root.to_str().unwrap(), "git")
            .unwrap();
        let fork = store
            .upsert_project_fork(
                "demo",
                "",
                "https://github.com/alice/widget.git",
                "fork",
                "alice",
            )
            .unwrap();

        let checks = check_fork_health(&store, &fork);
        assert!(checks.iter().all(|c| c.name != "fork-remote"));

        let _ = std::fs::remove_dir_all(&root);
    }

    /// A local `url.<x>.insteadOf` rewrite rule (e.g. a contributor who
    /// prefers SSH auth and rewrites every `https://github.com/` remote to
    /// an SSH form) makes `git remote get-url` return a URL that differs
    /// from the one that was actually registered, even though the remote is
    /// perfectly correct. Both the health check and `ensure_fork_remote`
    /// must compare against the raw configured value instead, or every user
    /// with such a rule would see a permanent false "mismatch".
    #[test]
    fn a_local_url_rewrite_rule_does_not_produce_a_false_remote_mismatch() {
        let root = tmp_dir("remote-rewrite");
        crate::guardian_merge::git(&root, &["init"]).unwrap();
        crate::guardian_merge::git(
            &root,
            &[
                "remote",
                "add",
                "fork",
                "https://github.com/alice/widget.git",
            ],
        )
        .unwrap();
        crate::guardian_merge::git(
            &root,
            &[
                "config",
                "url.ssh://git@github.com/.insteadOf",
                "https://github.com/",
            ],
        )
        .unwrap();
        assert!(
            crate::guardian_merge::git(&root, &["remote", "get-url", "fork"])
                .unwrap()
                .trim()
                .starts_with("ssh://"),
            "test setup didn't actually trigger a rewrite"
        );

        let store = Store::open_in_memory().unwrap();
        store
            .register_project("demo", "d", root.to_str().unwrap(), "git")
            .unwrap();
        let fork = store
            .upsert_project_fork(
                "demo",
                "",
                "https://github.com/alice/widget.git",
                "fork",
                "alice",
            )
            .unwrap();

        let checks = check_fork_health(&store, &fork);
        assert!(checks.iter().all(|c| c.name != "fork-remote"));
        assert!(ensure_fork_remote(&root, "fork", &fork.fork_url).is_ok());
        assert_eq!(
            crate::guardian_merge::git(&root, &["config", "--get", "remote.fork.url"])
                .unwrap()
                .trim(),
            fork.fork_url
        );

        let _ = std::fs::remove_dir_all(&root);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn upsert_is_idempotent_and_preserves_created_at() {
        let store = Store::open_in_memory().unwrap();
        let first = store
            .upsert_project_fork(
                "proj",
                "alice",
                "git@github.com:alice/proj.git",
                "fork-alice",
                "alice",
            )
            .unwrap();
        std::thread::sleep(std::time::Duration::from_millis(2));
        let second = store
            .upsert_project_fork(
                "proj",
                "alice",
                "git@github.com:alice/proj-renamed.git",
                "fork-alice",
                "alice",
            )
            .unwrap();
        assert_eq!(first.created_at_ms, second.created_at_ms);
        assert_eq!(second.fork_url, "git@github.com:alice/proj-renamed.git");
        assert!(second.updated_at_ms >= first.updated_at_ms);
    }

    #[test]
    fn patch_only_changes_given_fields() {
        let store = Store::open_in_memory().unwrap();
        store
            .upsert_project_fork("proj", "alice", "url-1", "remote-1", "alice")
            .unwrap();
        let patched = store
            .patch_project_fork("proj", "alice", Some("url-2"), None, None)
            .unwrap();
        assert_eq!(patched.fork_url, "url-2");
        assert_eq!(patched.remote_name, "remote-1");
        assert_eq!(patched.fork_owner, "alice");
    }

    #[test]
    fn patch_on_missing_row_is_not_found() {
        let store = Store::open_in_memory().unwrap();
        assert!(matches!(
            store.patch_project_fork("proj", "nobody", Some("url"), None, None),
            Err(StoreError::NotFound)
        ));
    }

    #[test]
    fn resolve_fork_uses_only_the_named_users_row() {
        let store = Store::open_in_memory().unwrap();
        store
            .upsert_project_fork("proj", "", "default-url", "fork", "")
            .unwrap();
        store
            .upsert_project_fork("proj", "alice", "alice-url", "fork-alice", "alice")
            .unwrap();

        assert_eq!(
            store
                .resolve_fork("proj", "alice")
                .unwrap()
                .unwrap()
                .fork_url,
            "alice-url"
        );
        assert!(store.resolve_fork("proj", "bob").unwrap().is_none());
    }

    #[test]
    fn resolve_fork_on_a_project_with_no_registered_fork_is_none() {
        let store = Store::open_in_memory().unwrap();
        assert!(store.resolve_fork("proj", "alice").unwrap().is_none());
    }

    #[test]
    fn fork_row_survives_user_deletion() {
        let store = Store::open_in_memory().unwrap();
        store.create_user("alice").unwrap();
        store
            .upsert_project_fork("proj", "alice", "alice-url", "fork-alice", "alice")
            .unwrap();
        store.delete_user("alice").unwrap();
        assert!(store.get_project_fork("proj", "alice").unwrap().is_some());
    }

    #[test]
    fn list_by_project_and_by_user() {
        let store = Store::open_in_memory().unwrap();
        store
            .upsert_project_fork("proj-a", "alice", "u1", "r1", "alice")
            .unwrap();
        store
            .upsert_project_fork("proj-b", "alice", "u2", "r2", "alice")
            .unwrap();
        store
            .upsert_project_fork("proj-a", "bob", "u3", "r3", "bob")
            .unwrap();

        assert_eq!(
            store
                .list_project_forks_for_project("proj-a")
                .unwrap()
                .len(),
            2
        );
        assert_eq!(store.list_project_forks_for_user("alice").unwrap().len(), 2);
        assert_eq!(store.list_project_forks().unwrap().len(), 3);
    }

    #[test]
    fn delete_reports_whether_a_row_existed() {
        let store = Store::open_in_memory().unwrap();
        assert!(!store.delete_project_fork("proj", "alice").unwrap());
        store
            .upsert_project_fork("proj", "alice", "url", "remote", "alice")
            .unwrap();
        assert!(store.delete_project_fork("proj", "alice").unwrap());
        assert!(store.get_project_fork("proj", "alice").unwrap().is_none());
    }

    #[test]
    fn default_remote_name_is_fork_for_default_row_and_sanitized_otherwise() {
        assert_eq!(default_remote_name(""), "fork");
        assert_eq!(default_remote_name("alice"), "fork-alice");
        assert_eq!(default_remote_name("a.weird name!"), "fork-a.weird-name-");
    }

    #[test]
    fn upsert_with_identity_round_trips_and_a_later_plain_upsert_preserves_it() {
        let store = Store::open_in_memory().unwrap();
        let created = store
            .upsert_project_fork_with_identity(
                "proj",
                "alice",
                "url-1",
                "fork-alice",
                "alice",
                Some("Alice Example"),
                Some("alice@example.com"),
            )
            .unwrap();
        assert_eq!(created.git_user_name.as_deref(), Some("Alice Example"));
        assert_eq!(created.git_user_email.as_deref(), Some("alice@example.com"));

        // A later plain `upsert_project_fork` call (no identity args, e.g. an
        // older caller just updating `fork_url`) must not silently wipe out
        // an already-configured identity.
        let updated = store
            .upsert_project_fork("proj", "alice", "url-2", "fork-alice", "alice")
            .unwrap();
        assert_eq!(updated.fork_url, "url-2");
        assert_eq!(updated.git_user_name.as_deref(), Some("Alice Example"));
        assert_eq!(updated.git_user_email.as_deref(), Some("alice@example.com"));
    }

    #[test]
    fn a_freshly_upserted_fork_has_no_identity_by_default() {
        let store = Store::open_in_memory().unwrap();
        let fork = store
            .upsert_project_fork("proj", "alice", "url", "fork-alice", "alice")
            .unwrap();
        assert_eq!(fork.git_user_name, None);
        assert_eq!(fork.git_user_email, None);
    }

    #[test]
    fn patch_ex_changes_only_the_given_identity_fields() {
        let store = Store::open_in_memory().unwrap();
        store
            .upsert_project_fork_with_identity(
                "proj",
                "alice",
                "url",
                "fork-alice",
                "alice",
                Some("Alice Example"),
                Some("alice@example.com"),
            )
            .unwrap();
        let patched = store
            .patch_project_fork_ex(
                "proj",
                "alice",
                None,
                None,
                None,
                Some("Alice Renamed"),
                None,
            )
            .unwrap();
        assert_eq!(patched.git_user_name.as_deref(), Some("Alice Renamed"));
        assert_eq!(patched.git_user_email.as_deref(), Some("alice@example.com"));
    }
}
