//! Ark manages registered ralphus worktrees after their owning entities age
//! out. The periodic path only detects and escalates; deletion is an explicit
//! operation until the rollout gate is deliberately changed.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex};

use rusqlite::{OptionalExtension, params};

use crate::cancel::Cancellations;
use crate::config::ArkConfig;
use crate::mailbox::MailboxPriority;
use crate::scheduler::Semaphore;
use crate::store::{SquadState, Store, now_ms};

/// Automatic sweeps must use `DetectOnly`. `Reap` exists for a future manual
/// administrative entry point and for focused tests of the complete path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SweepMode {
    DetectOnly,
    Reap,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Owner {
    Squad(String),
    Guardian(String),
}

impl Owner {
    fn kind(&self) -> &'static str {
        match self {
            Self::Squad(_) => "squad",
            Self::Guardian(_) => "review",
        }
    }
    fn id(&self) -> &str {
        match self {
            Self::Squad(id) | Self::Guardian(id) => id,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Candidate {
    pub project_root: PathBuf,
    pub worktree: PathBuf,
    pub owner: Owner,
    /// Every cancellation-registry key whose entity references this path.
    pub claim_keys: Vec<String>,
    pub updated_at_ms: i64,
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct SweepReport {
    pub registered: usize,
    pub stale: usize,
    pub notified: usize,
    pub reaped: usize,
    pub skipped: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReapedWorktree {
    pub preserved_ref: String,
    pub remote_ref: String,
}

fn is_ralphus_worktree(path: &Path) -> bool {
    let normalized = path
        .to_string_lossy()
        .replace('\\', "/")
        .to_ascii_lowercase();
    normalized.contains("/.git/.ralphus/w/") || normalized.contains("/.git/.ralphus/g/")
}

/// Canonicalize `path` when it exists on disk before stringifying it, so two
/// different textual spellings of the same real directory always produce the
/// same key. This matters on Windows specifically: `git worktree list
/// --porcelain` always resolves to the long filename form, but a path built
/// in-process from an env var like `TEMP`/`TMP` can be the short 8.3-alias
/// form instead (observed on GitHub Actions' Windows runners, e.g.
/// `RUNNER~1` vs. the real account name) -- without resolving both sides to
/// the same canonical form first, a squad's own `cwd` would never match its
/// worktree's registered path here, and every candidate detection using this
/// join would silently come up empty. Falls back to the raw path when it no
/// longer exists on disk (already cleaned up, or a synthetic path in tests)
/// so canonicalization failure never turns into a hard error here.
fn normalized(path: &Path) -> String {
    let resolved = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    let text = resolved.to_string_lossy();
    // `canonicalize` on Windows always returns the `\\?\`-prefixed
    // extended-length form; strip it so the result matches the plain
    // (non-prefixed) style every other path in this module already uses.
    let text = text.strip_prefix(r"\\?\").unwrap_or(&text);
    text.replace('\\', "/")
        .trim_end_matches('/')
        .to_ascii_lowercase()
}

fn containing_worktree<'a>(cwd: &str, registered: &'a HashMap<String, PathBuf>) -> Option<&'a str> {
    registered
        .keys()
        .filter(|root| {
            cwd == root.as_str()
                || cwd
                    .strip_prefix(root.as_str())
                    .is_some_and(|tail| tail.starts_with('/'))
        })
        .max_by_key(|root| root.len())
        .map(String::as_str)
}

fn git(root: &Path, args: &[&str]) -> Result<String, String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(args)
        .output()
        .map_err(|e| format!("could not run git: {e}"))?;
    if output.status.success() {
        Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
    } else {
        Err(String::from_utf8_lossy(&output.stderr).trim().to_string())
    }
}

fn registered_worktrees(root: &Path) -> Result<Vec<PathBuf>, String> {
    let text = git(root, &["worktree", "list", "--porcelain"])?;
    Ok(text
        .lines()
        .filter_map(|line| line.strip_prefix("worktree "))
        .map(PathBuf::from)
        .filter(|path| is_ralphus_worktree(path))
        .collect())
}

fn registered_count(root: &Path) -> Result<usize, String> {
    Ok(git(root, &["worktree", "list", "--porcelain"])?
        .lines()
        .filter(|line| line.starts_with("worktree "))
        .count())
}

struct RecoveryInfo {
    head: String,
    remote_ref: String,
}

fn remote_recovery_info(worktree: &Path, project_root: &Path) -> Result<RecoveryInfo, String> {
    if !git(worktree, &["status", "--porcelain"])?.is_empty() {
        return Err("worktree has uncommitted changes".to_string());
    }
    let head = git(worktree, &["rev-parse", "HEAD"])?;
    let upstream = git(
        worktree,
        &["rev-parse", "--symbolic-full-name", "@{upstream}"],
    )?;
    let Some(remote_tail) = upstream.strip_prefix("refs/remotes/") else {
        return Err(format!("upstream {upstream} is not a remote-tracking ref"));
    };
    let Some((remote, branch)) = remote_tail.split_once('/') else {
        return Err(format!("cannot parse remote upstream {upstream}"));
    };
    let remote_output = git(
        project_root,
        &[
            "ls-remote",
            "--exit-code",
            remote,
            &format!("refs/heads/{branch}"),
        ],
    )?;
    let remote_head = remote_output
        .split_whitespace()
        .next()
        .ok_or_else(|| "remote branch returned no commit".to_string())?;
    git(
        project_root,
        &["merge-base", "--is-ancestor", &head, remote_head],
    )?;
    Ok(RecoveryInfo {
        head,
        remote_ref: format!("{remote}/{branch}"),
    })
}

/// Derive stale candidates exclusively from daemon state, then intersect with
/// git's live registry. A task worktree shared by multiple squads is eligible
/// only when every known owner is terminal and old.
struct StoreSnapshot {
    guardians: Vec<(String, String, i64, String)>,
    squads: Vec<(String, String, i64, String)>,
}

fn store_snapshot(store: &Store, project_root: &Path) -> crate::store::Result<StoreSnapshot> {
    let mut stmt = store.conn.prepare(
        "SELECT g.id, g.status, g.updated_at_ms, gb.worktree
         FROM guardians g JOIN guardian_branches gb ON gb.guardian_id=g.id
         WHERE g.git_root=?1 AND gb.worktree IS NOT NULL
         UNION ALL
         SELECT id, status, updated_at_ms, combined_worktree FROM guardians
         WHERE git_root=?1 AND combined_worktree IS NOT NULL",
    )?;
    let guardians = stmt
        .query_map(params![project_root.to_string_lossy()], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))
        })?
        .collect::<Result<Vec<_>, _>>()?;
    let mut stmt = store.conn.prepare(
        "SELECT s.id, s.state, s.updated_at_ms, c.cwd FROM squads s
         JOIN cells c ON c.squad_id=s.id WHERE c.cwd IS NOT NULL",
    )?;
    let squads = stmt
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(StoreSnapshot { guardians, squads })
}

pub fn detect(
    store: &Store,
    project_root: &Path,
    cfg: &ArkConfig,
) -> crate::store::Result<Vec<Candidate>> {
    let snapshot = store_snapshot(store, project_root)?;
    Ok(detect_snapshot(snapshot, project_root, cfg))
}

fn detect_snapshot(
    snapshot: StoreSnapshot,
    project_root: &Path,
    cfg: &ArkConfig,
) -> Vec<Candidate> {
    let cutoff = now_ms().saturating_sub(cfg.stale_after_ms());
    let registered: HashMap<String, PathBuf> = registered_worktrees(project_root)
        .unwrap_or_default()
        .into_iter()
        .map(|p| (normalized(&p), p))
        .collect();
    let mut candidates = Vec::new();

    for (id, status, updated, path) in snapshot.guardians {
        if matches!(status.as_str(), "cancelled" | "deployed") && updated <= cutoff {
            if let Some(actual) = registered.get(&normalized(Path::new(&path))) {
                if remote_recovery_info(actual, project_root).is_ok() {
                    candidates.push(Candidate {
                        project_root: project_root.to_path_buf(),
                        worktree: actual.clone(),
                        owner: Owner::Guardian(id.clone()),
                        claim_keys: vec![format!("guardian:{id}")],
                        updated_at_ms: updated,
                    });
                }
            }
        }
    }

    let mut by_path: BTreeMap<String, Vec<(String, String, i64)>> = BTreeMap::new();
    for (id, state, updated, path) in snapshot.squads {
        let cwd = normalized(Path::new(&path));
        if let Some(key) = containing_worktree(&cwd, &registered) {
            by_path
                .entry(key.to_string())
                .or_default()
                .push((id, state, updated));
        }
    }
    for (path, owners) in by_path {
        let safe = owners.iter().all(|(_, state, updated)| {
            SquadState::parse(state).is_some_and(SquadState::is_terminal) && *updated <= cutoff
        });
        if safe {
            let (id, _, updated) = owners
                .iter()
                .max_by_key(|(_, _, at)| at)
                .expect("nonempty owners");
            let claim_keys = owners.iter().map(|(id, _, _)| id.clone()).collect();
            let worktree = &registered[&path];
            if remote_recovery_info(worktree, project_root).is_ok() {
                candidates.push(Candidate {
                    project_root: project_root.to_path_buf(),
                    worktree: worktree.clone(),
                    owner: Owner::Squad(id.clone()),
                    claim_keys,
                    updated_at_ms: *updated,
                });
            }
        }
    }
    candidates.sort_by_key(|c| c.updated_at_ms);
    candidates.dedup_by(|a, b| normalized(&a.worktree) == normalized(&b.worktree));
    candidates
}

fn notify_old_reviews(
    store: &Store,
    project_root: &Path,
    cfg: &ArkConfig,
) -> crate::store::Result<usize> {
    let cutoff = now_ms().saturating_sub(cfg.stale_after_ms());
    let mut stmt = store.conn.prepare(
        "SELECT id, name, status, updated_at_ms FROM guardians
         WHERE git_root=?1 AND updated_at_ms<=?2",
    )?;
    let rows = stmt
        .query_map(params![project_root.to_string_lossy(), cutoff], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, i64>(3)?,
            ))
        })?
        .collect::<Result<Vec<_>, _>>()?;
    let mut count = 0;
    for (id, name, status, _) in rows {
        if store.claim_ark_notification("review", &id)? {
            store.enqueue_mailbox_message(MailboxPriority::High,
                &format!("Ark found old review {id} ({name}) in {status}; inspect it before any worktree cleanup."),
                None, None, None)?;
            count += 1;
        }
    }
    Ok(count)
}

fn ref_component(value: &str) -> String {
    value
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.') {
                c
            } else {
                '-'
            }
        })
        .collect()
}

/// Pin and remove one worktree. Cleanliness, remote reachability, active
/// ownership, and global permit idleness are all hard preconditions.
pub fn reap(
    candidate: &Candidate,
    cancellations: &Cancellations,
    sem: &Semaphore,
) -> Result<ReapedWorktree, String> {
    if candidate
        .claim_keys
        .iter()
        .any(|key| cancellations.is_active(key))
    {
        return Err(format!("{} is actively claimed", candidate.owner.id()));
    }
    if sem.in_use() != 0 {
        return Err(
            "a semaphore permit is active; Ark conservatively defers all reaping".to_string(),
        );
    }
    let recovery = remote_recovery_info(&candidate.worktree, &candidate.project_root)?;

    let preserved_ref = format!(
        "refs/ralphus/ark/{}/{}/{}",
        candidate.owner.kind(),
        ref_component(candidate.owner.id()),
        now_ms()
    );
    git(
        &candidate.project_root,
        &["update-ref", &preserved_ref, &recovery.head],
    )?;
    if let Err(error) = git(
        &candidate.project_root,
        &[
            "worktree",
            "remove",
            candidate.worktree.to_string_lossy().as_ref(),
        ],
    ) {
        return Err(format!(
            "preserved {preserved_ref}, but worktree removal failed: {error}"
        ));
    }
    Ok(ReapedWorktree {
        preserved_ref,
        remote_ref: recovery.remote_ref,
    })
}

/// Sweep every registered git project. Detection and deduplicated review-age
/// escalation always run. Reaping is additionally constrained by each
/// project's ceiling and only happens in explicit `Reap` mode.
pub fn sweep(
    store: &Arc<Mutex<Store>>,
    cancellations: &Cancellations,
    sem: &Arc<Semaphore>,
    mode: SweepMode,
) -> SweepReport {
    sweep_inner(store, cancellations, sem, mode, false)
}

/// Scheduler entry point. Each project is considered on its own configured
/// cadence, persisted across daemon restarts. It always uses the deletion-off
/// mode.
pub fn periodic_sweep(
    store: &Arc<Mutex<Store>>,
    cancellations: &Cancellations,
    sem: &Arc<Semaphore>,
) -> SweepReport {
    sweep_inner(store, cancellations, sem, SweepMode::DetectOnly, true)
}

fn sweep_inner(
    store: &Arc<Mutex<Store>>,
    cancellations: &Cancellations,
    sem: &Arc<Semaphore>,
    mode: SweepMode,
    only_due: bool,
) -> SweepReport {
    let projects = store
        .lock()
        .expect("store mutex poisoned")
        .list_projects()
        .unwrap_or_default();
    let mut report = SweepReport::default();
    for project in projects.into_iter().filter(|p| p.vcs == "git") {
        let root = PathBuf::from(&project.path);
        let cfg = crate::config::load_ark_config(&root);
        if let Err(error) = cfg.validate() {
            report.skipped.push(format!("{}: {error}", project.name));
            continue;
        }
        if only_due {
            let due = {
                let guard = store.lock().expect("store mutex poisoned");
                let last: Option<i64> = guard
                    .conn
                    .query_row(
                        "SELECT swept_at_ms FROM ark_sweeps WHERE project_path=?1",
                        params![project.path],
                        |row| row.get(0),
                    )
                    .optional()
                    .unwrap_or(None);
                let interval_ms =
                    i64::try_from(cfg.sweep_interval().as_millis()).unwrap_or(i64::MAX);
                last.is_none_or(|at| now_ms().saturating_sub(at) >= interval_ms)
            };
            if !due {
                continue;
            }
        }
        let (snapshot, notified) = {
            let guard = store.lock().expect("store mutex poisoned");
            (
                store_snapshot(&guard, &root).ok(),
                notify_old_reviews(&guard, &root, &cfg).unwrap_or(0),
            )
        };
        // Git registry scans and remote probes can be slow. They deliberately
        // run after releasing the daemon's store mutex.
        let candidates = snapshot
            .map(|snapshot| detect_snapshot(snapshot, &root, &cfg))
            .unwrap_or_default();
        let registered = registered_count(&root).unwrap_or(0);
        report.registered += registered;
        report.stale += candidates.len();
        report.notified += notified;
        if only_due {
            let guard = store.lock().expect("store mutex poisoned");
            let _ = guard.conn.execute(
                "INSERT INTO ark_sweeps(project_path, swept_at_ms) VALUES(?1, ?2)
                 ON CONFLICT(project_path) DO UPDATE SET swept_at_ms=excluded.swept_at_ms",
                params![project.path, now_ms()],
            );
        }
        if mode == SweepMode::Reap && registered > cfg.max_worktrees {
            let needed = registered - cfg.max_worktrees;
            for candidate in candidates.iter().take(needed) {
                match reap(candidate, cancellations, sem) {
                    Ok(_) => report.reaped += 1,
                    Err(error) => report
                        .skipped
                        .push(format!("{}: {error}", candidate.worktree.display())),
                }
            }
        }
    }
    if let Ok(guard) = store.lock() {
        crate::cartographer::Note::new("ark").emit(
            &guard,
            format!(
                "Ark sweep found {} stale worktrees and reaped {}",
                report.stale, report.reaped
            ),
            serde_json::json!({
                "registered": report.registered,
                "stale": report.stale,
                "notified": report.notified,
                "reaped": report.reaped,
                "mode": format!("{mode:?}"),
            }),
        );
    }
    report
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT_DIR: AtomicU64 = AtomicU64::new(0);

    fn temp_dir() -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "ralphus-ark-{}-{}",
            std::process::id(),
            NEXT_DIR.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&path).unwrap();
        path
    }

    fn run(root: &Path, args: &[&str]) {
        let out = Command::new("git")
            .arg("-C")
            .arg(root)
            .args(args)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    fn repo_with_pushed_worktree() -> (PathBuf, PathBuf) {
        let base = temp_dir();
        let remote = base.join("remote.git");
        std::fs::create_dir_all(&remote).unwrap();
        run(&remote, &["init", "--bare"]);
        let repo = base.join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        run(&repo, &["init", "-b", "main"]);
        run(&repo, &["config", "user.email", "ark@example.invalid"]);
        run(&repo, &["config", "user.name", "Ark Test"]);
        std::fs::write(repo.join("seed"), "seed").unwrap();
        run(&repo, &["add", "seed"]);
        run(&repo, &["commit", "-m", "seed"]);
        run(
            &repo,
            &["remote", "add", "origin", remote.to_str().unwrap()],
        );
        run(&repo, &["push", "-u", "origin", "main"]);
        run(&repo, &["branch", "feature"]);
        let wt = repo.join(".git").join(".ralphus").join("w").join("feature");
        std::fs::create_dir_all(wt.parent().unwrap()).unwrap();
        run(&repo, &["worktree", "add", wt.to_str().unwrap(), "feature"]);
        run(&wt, &["push", "-u", "origin", "feature"]);
        (repo, wt)
    }

    #[test]
    fn recognizes_both_ark_worktree_families() {
        assert!(is_ralphus_worktree(Path::new("C:/r/.git/.ralphus/w/a")));
        assert!(is_ralphus_worktree(Path::new(
            "C:/r/.git/.ralphus/g/1/wt-a"
        )));
        assert!(!is_ralphus_worktree(Path::new("C:/r/.git/worktrees/a")));
        let registered = HashMap::from([(
            "c:/r/.git/.ralphus/w/a".to_string(),
            PathBuf::from("C:/r/.git/.ralphus/w/a"),
        )]);
        assert_eq!(
            containing_worktree("c:/r/.git/.ralphus/w/a/nested/src", &registered),
            Some("c:/r/.git/.ralphus/w/a")
        );
    }

    #[test]
    fn ref_components_cannot_escape_namespace() {
        assert_eq!(ref_component("squad/a b"), "squad-a-b");
    }

    #[test]
    fn reap_preserves_ark_ref_and_requires_remote_recovery() {
        let (repo, wt) = repo_with_pushed_worktree();
        let candidate = Candidate {
            project_root: repo.clone(),
            worktree: wt.clone(),
            owner: Owner::Squad("squad-1".to_string()),
            claim_keys: vec!["squad-1".to_string()],
            updated_at_ms: 0,
        };
        let result = reap(&candidate, &Cancellations::new(), &Semaphore::new(1)).unwrap();
        assert!(
            result
                .preserved_ref
                .starts_with("refs/ralphus/ark/squad/squad-1/")
        );
        assert_eq!(result.remote_ref, "origin/feature");
        assert!(!wt.exists());
        assert!(
            !git(&repo, &["rev-parse", "--verify", &result.preserved_ref])
                .unwrap()
                .is_empty()
        );
        std::fs::remove_dir_all(repo.parent().unwrap()).unwrap();
    }

    #[test]
    fn reap_rejects_a_clean_local_only_commit() {
        let (repo, wt) = repo_with_pushed_worktree();
        std::fs::write(wt.join("ark-only.txt"), "not pushed").unwrap();
        run(&wt, &["add", "ark-only.txt"]);
        run(&wt, &["commit", "-m", "local only"]);
        let candidate = Candidate {
            project_root: repo.clone(),
            worktree: wt.clone(),
            owner: Owner::Squad("squad-local".to_string()),
            claim_keys: vec!["squad-local".to_string()],
            updated_at_ms: 0,
        };
        assert!(reap(&candidate, &Cancellations::new(), &Semaphore::new(1)).is_err());
        assert!(wt.exists());
        run(&repo, &["worktree", "remove", wt.to_str().unwrap()]);
        std::fs::remove_dir_all(repo.parent().unwrap()).unwrap();
    }

    #[test]
    fn reap_refuses_active_owner_or_any_active_permit() {
        let candidate = Candidate {
            project_root: PathBuf::from("missing"),
            worktree: PathBuf::from("missing"),
            owner: Owner::Guardian("guardian-1".to_string()),
            claim_keys: vec!["guardian:guardian-1".to_string()],
            updated_at_ms: 0,
        };
        let cancellations = Cancellations::new();
        let _token = cancellations.register("guardian:guardian-1");
        assert!(
            reap(&candidate, &cancellations, &Semaphore::new(1))
                .unwrap_err()
                .contains("actively claimed")
        );
        cancellations.remove("guardian:guardian-1");
        let sem = Semaphore::new(1);
        let _permit = sem.acquire();
        assert!(
            reap(&candidate, &cancellations, &sem)
                .unwrap_err()
                .contains("semaphore permit")
        );
    }

    #[test]
    fn detection_requires_every_squad_sharing_a_worktree_to_be_old_and_terminal() {
        let (repo, wt) = repo_with_pushed_worktree();
        let store = Store::open_in_memory().unwrap();
        for (id, state) in [("squad-old", "done"), ("squad-live", "running")] {
            store.conn.execute(
                "INSERT INTO squads(id, state, created_at_ms, updated_at_ms) VALUES(?1, ?2, 0, 0)",
                params![id, state],
            ).unwrap();
            store
                .conn
                .execute(
                    "INSERT INTO tasks(squad_id, idx, name, state) VALUES(?1, 0, 'task', ?2)",
                    params![id, state],
                )
                .unwrap();
            store
                .conn
                .execute(
                    "INSERT INTO cells(squad_id, task_idx, idx, sid, cwd, agent, state)
                 VALUES(?1, 0, 0, 'cell', ?2, 'raw', ?3)",
                    params![id, wt.to_string_lossy(), state],
                )
                .unwrap();
        }
        let cfg = ArkConfig {
            stale_after_days: 1,
            ..ArkConfig::default()
        };
        assert!(detect(&store, &repo, &cfg).unwrap().is_empty());
        store
            .conn
            .execute("UPDATE squads SET state='done' WHERE id='squad-live'", [])
            .unwrap();
        let found = detect(&store, &repo, &cfg).unwrap();
        assert_eq!(found.len(), 1);
        // Compare canonicalized forms, not raw `PathBuf` equality: on a
        // runner whose `TEMP`/`TMP` resolves to a Windows short (8.3) alias,
        // `wt` (built from that env var) and `found[0].worktree` (git's own
        // long-form output, itself now also normalized -- see
        // `normalized`'s doc comment) name the same directory but spell it
        // differently.
        assert_eq!(normalized(&found[0].worktree), normalized(&wt));
        run(
            &repo,
            &["worktree", "remove", found[0].worktree.to_str().unwrap()],
        );
        std::fs::remove_dir_all(repo.parent().unwrap()).unwrap();
    }

    #[test]
    fn old_open_review_notifies_once_but_is_not_a_reap_candidate() {
        let store = Store::open_in_memory().unwrap();
        let root = temp_dir();
        store.conn.execute(
            "INSERT INTO guardians(id, name, base_branch, git_root, status, created_at_ms, updated_at_ms)
             VALUES('guardian-1', 'old open', 'main', ?1, 'in_review', 0, 0)",
            params![root.to_string_lossy()],
        ).unwrap();
        let cfg = ArkConfig {
            stale_after_days: 1,
            ..ArkConfig::default()
        };
        assert_eq!(notify_old_reviews(&store, &root, &cfg).unwrap(), 1);
        assert_eq!(notify_old_reviews(&store, &root, &cfg).unwrap(), 0);
        let client = store.register_mailbox_client().unwrap();
        assert_eq!(
            store
                .mailbox_messages_for_client(&client, true, None)
                .unwrap()
                .len(),
            1
        );
        assert!(detect(&store, &root, &cfg).unwrap().is_empty());
        std::fs::remove_dir_all(root).unwrap();
    }
}
