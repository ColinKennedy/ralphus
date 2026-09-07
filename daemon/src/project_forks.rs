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
    pub created_at_ms: i64,
    pub updated_at_ms: i64,
}

const FORK_COLUMNS: &str =
    "project, user, fork_url, remote_name, fork_owner, created_at_ms, updated_at_ms";

fn row_to_fork_record(r: &rusqlite::Row<'_>) -> rusqlite::Result<ForkRecord> {
    Ok(ForkRecord {
        project: r.get(0)?,
        user: r.get(1)?,
        fork_url: r.get(2)?,
        remote_name: r.get(3)?,
        fork_owner: r.get(4)?,
        created_at_ms: r.get(5)?,
        updated_at_ms: r.get(6)?,
    })
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
        let now = now_ms();
        self.conn.execute(
            "INSERT INTO project_forks(project, user, fork_url, remote_name, fork_owner, created_at_ms, updated_at_ms)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?6)
             ON CONFLICT(project, user) DO UPDATE SET
                fork_url = excluded.fork_url,
                remote_name = excluded.remote_name,
                fork_owner = excluded.fork_owner,
                updated_at_ms = excluded.updated_at_ms",
            rusqlite::params![project, user, fork_url, remote_name, fork_owner, now],
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
        let Some(existing) = self.get_project_fork(project, user)? else {
            return Err(StoreError::NotFound);
        };
        let fork_url = fork_url.unwrap_or(&existing.fork_url);
        let remote_name = remote_name.unwrap_or(&existing.remote_name);
        let fork_owner = fork_owner.unwrap_or(&existing.fork_owner);
        let now = now_ms();
        self.conn.execute(
            "UPDATE project_forks SET fork_url=?1, remote_name=?2, fork_owner=?3, updated_at_ms=?4
             WHERE project=?5 AND user=?6",
            rusqlite::params![fork_url, remote_name, fork_owner, now, project, user],
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

    /// Resolve which fork row applies for `(project, user)`: the exact
    /// claimed-user row first, then the project-wide `user=""` default, else
    /// `None` when the project has no registered fork at all.
    ///
    /// # Errors
    /// Propagates any SQLite failure.
    pub fn resolve_fork(&self, project: &str, user: &str) -> StoreResult<Option<ForkRecord>> {
        if !user.is_empty() {
            if let Some(row) = self.get_project_fork(project, user)? {
                return Ok(Some(row));
            }
        }
        self.get_project_fork(project, "")
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
    let mut out = Vec::new();
    if !fork.user.is_empty() && matches!(store.get_user(&fork.user), Ok(None)) {
        out.push(check(
            &fork.project,
            &fork.user,
            "fork-user",
            "warn",
            format!(
                "fork registered for user {:?}, who is no longer registered -- the row \
                 still applies to that name if resolved (RAL-338: rows deliberately \
                 survive user deletion), but the board should flag it as orphaned",
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
            return out;
        }
        Err(e) => {
            out.push(check(
                &fork.project,
                &fork.user,
                "fork-project",
                "fail",
                e.to_string(),
            ));
            return out;
        }
    };
    let root = std::path::Path::new(&project.path);
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
                "local remote {:?} in {:?} points at {:?}, not the registered fork_url {:?} -- \
                 the next submission through this fork will correct it automatically",
                fork.remote_name,
                project.path,
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
                "no local git remote named {:?} configured in {:?} yet -- it will be created \
                 automatically the next time this fork is used to submit",
                fork.remote_name, project.path
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
                    "fork is only indirectly related to the parent (a fork of a fork, or a \
                     sibling) -- cross-repository behavior is only proven for a direct fork",
                ),
                crate::forge::ForkRelationship::NoRelationship => (
                    "fail",
                    "fork does not appear to be forge-related to the parent at all -- \
                     submission through it will be blocked without --allow-unlinked-fork",
                ),
                crate::forge::ForkRelationship::NotVisible => (
                    "warn",
                    "could not confirm the fork relationship over the forge API (private, \
                     deleted, or missing token) -- this is not proof of no relationship",
                ),
                crate::forge::ForkRelationship::CrossInstance => (
                    "fail",
                    "the fork and its parent are on different forge instances/kinds -- there \
                     is no cross-repository PR path between them",
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
    fn resolve_fork_prefers_exact_user_then_falls_back_to_default() {
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
        assert_eq!(
            store.resolve_fork("proj", "bob").unwrap().unwrap().fork_url,
            "default-url"
        );
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
}
