//! Guardian merge engine: build a linear review branch from the collected
//! feature branches by cherry-picking each one's commits onto a growing stack in
//! a dedicated worktree.
//!
//! Cherry-picking (rather than `git rebase` of the feature branches) keeps the
//! feature branches themselves untouched. When a branch conflicts, a `Runner`
//! (the same agent runner the scheduler uses) is asked to edit the conflicted
//! files marker-free, after which the pick continues; a branch that still cannot
//! be resolved marks the guardian `MergeFailed`.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex};

use crate::guardian::{GuardianStatus, MergeStatus};
use crate::runner::{Runner, RunnerSpec};
use crate::server::Reply;
use crate::store::Store;

/// Run `git` with `args` in `root`, returning stdout on success or a message.
/// `GIT_EDITOR=true` keeps operations like `cherry-pick --continue` from opening
/// an interactive editor.
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

/// Files with unresolved merge conflicts in a worktree.
fn conflicted_files(wt: &Path) -> Vec<String> {
    git(wt, &["diff", "--name-only", "--diff-filter=U"])
        .unwrap_or_default()
        .lines()
        .map(str::to_string)
        .collect()
}

/// Count remaining `<<<<<<<` conflict markers across the given files.
fn count_markers(wt: &Path, files: &[String]) -> usize {
    files
        .iter()
        .map(|f| {
            std::fs::read_to_string(wt.join(f))
                .map(|c| c.lines().filter(|l| l.starts_with("<<<<<<<")).count())
                .unwrap_or(0)
        })
        .sum()
}

/// The agent backend + model used to resolve conflicts (env-configurable).
fn resolver_agent() -> String {
    std::env::var("RALPHUS_RESOLVER_AGENT").unwrap_or_else(|_| "ollama".to_string())
}
fn resolver_model() -> Option<String> {
    std::env::var("RALPHUS_RESOLVER_MODEL")
        .ok()
        .or_else(|| Some("qwen3:8b".to_string()))
}

/// Drive an agent to resolve the in-progress cherry-pick conflicts in `wt`,
/// then `git add` + `cherry-pick --continue`, looping until the pick completes
/// or a cap is hit. Returns Ok when fully resolved.
fn resolve_conflicts_with_agent(
    runner: &dyn Runner,
    wt: &Path,
    branch: &str,
) -> std::result::Result<(), String> {
    for _ in 0..32 {
        let files = conflicted_files(wt);
        if files.is_empty() {
            return Ok(()); // sequencer advanced with nothing left to resolve
        }
        let prompt = format!(
            "You are resolving git merge conflicts while integrating branch '{branch}'. \
             These files contain conflict markers (<<<<<<<, =======, >>>>>>>): {}. \
             Edit each file into a correct merged version that removes ALL conflict \
             markers while preserving the intent of both sides. Do not run any git commands.",
            files.join(", ")
        );
        let spec = RunnerSpec {
            run_id: "guardian".to_string(),
            task: "resolve".to_string(),
            session_id: "resolver".to_string(),
            cwd: wt.to_string_lossy().into_owned(),
            prompt: Some(prompt),
            command: None,
            agent: resolver_agent(),
            model: resolver_model(),
        };
        let _ = runner.run(&spec);

        let remaining = count_markers(wt, &files);
        if remaining > 0 {
            return Err(format!(
                "{remaining} conflict marker(s) remain after resolution"
            ));
        }
        git(wt, &["add", "-A"])?;
        // `--continue` fails if further commits in the range also conflict; the
        // loop re-checks and resolves those too.
        let _ = git(wt, &["cherry-pick", "--continue"]);
    }
    Err("exceeded conflict-resolution attempts".to_string())
}

/// The worktree directory a guardian's review stack is built in.
pub(crate) fn worktree_dir(git_root: &str, guardian_id: &str) -> PathBuf {
    Path::new(git_root)
        .join(".ralphus_guardian")
        .join(guardian_id)
}

/// Validate the guardian and kick off a background merge. Returns immediately.
pub fn start_merge(store: Arc<Mutex<Store>>, runner: Arc<dyn Runner>, id: &str) -> Reply {
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

    let sid = id.to_string();
    std::thread::spawn(move || run_merge(&store, runner.as_ref(), &sid));
    reply(202, "{\"status\":\"merging\"}")
}

/// Build the review stack for a guardian (synchronous; called on a worker thread
/// or directly in tests). Conflicts are resolved with `runner`.
pub fn run_merge(store: &Arc<Mutex<Store>>, runner: &dyn Runner, id: &str) {
    let guardian = match store.lock().expect("poisoned").get_guardian(id) {
        Ok(g) => g,
        Err(_) => return,
    };
    let root = PathBuf::from(&guardian.git_root);
    let review_branch = format!("guardian/{}", id.replace("guardian-", ""));
    let wt = worktree_dir(&guardian.git_root, id);

    let set_status = |s: GuardianStatus, detail: Option<&str>| {
        let _ = store
            .lock()
            .expect("poisoned")
            .set_guardian_status(id, s, detail);
    };
    set_status(GuardianStatus::Merging, None);

    // Fresh start: drop any prior worktree and review branch.
    let wt_str = wt.to_string_lossy().to_string();
    let _ = git(&root, &["worktree", "remove", "--force", &wt_str]);
    let _ = git(&root, &["branch", "-D", &review_branch]);
    if let Some(parent) = wt.parent() {
        let _ = std::fs::create_dir_all(parent);
    }

    if let Err(e) = git(
        &root,
        &[
            "worktree",
            "add",
            "-B",
            &review_branch,
            &wt_str,
            &guardian.base_branch,
        ],
    ) {
        set_status(GuardianStatus::MergeFailed, Some(&e));
        return;
    }

    let branches = store
        .lock()
        .expect("poisoned")
        .guardian_branches(id)
        .unwrap_or_default();
    for ob in &branches {
        let _ = store.lock().expect("poisoned").set_branch_status(
            id,
            ob.position,
            MergeStatus::InProgress,
            None,
        );

        match cherry_pick_branch(&wt, &guardian.base_branch, &ob.branch) {
            Ok(PickOutcome::Applied | PickOutcome::Empty) => {
                let _ = store.lock().expect("poisoned").set_branch_status(
                    id,
                    ob.position,
                    MergeStatus::Done,
                    None,
                );
            }
            Err(e) => {
                // A conflict leaves unmerged files; try to resolve with the agent.
                if conflicted_files(&wt).is_empty() {
                    let _ = git(&wt, &["cherry-pick", "--abort"]);
                    fail_branch(store, id, ob.position, &ob.branch, &e, &set_status);
                    return;
                }
                match resolve_conflicts_with_agent(runner, &wt, &ob.branch) {
                    Ok(()) => {
                        let _ = store.lock().expect("poisoned").set_branch_status(
                            id,
                            ob.position,
                            MergeStatus::ConflictResolved,
                            Some("resolved by agent"),
                        );
                    }
                    Err(re) => {
                        let _ = git(&wt, &["cherry-pick", "--abort"]);
                        fail_branch(store, id, ob.position, &ob.branch, &re, &set_status);
                        return;
                    }
                }
            }
        }
    }

    let _ = store
        .lock()
        .expect("poisoned")
        .set_guardian_review_branch(id, &review_branch);

    // Run the configured check gates in the assembled review worktree.
    let checks = store
        .lock()
        .expect("poisoned")
        .guardian_checks(id)
        .unwrap_or_default();
    for cmd in &checks {
        if !crate::verify::run_command_verify(&wt_str, cmd) {
            set_status(
                GuardianStatus::MergeFailed,
                Some(&format!("check failed: {cmd}")),
            );
            return;
        }
    }

    set_status(GuardianStatus::InReview, None);
}

/// Mark a branch failed and the guardian merge-failed with a reason.
fn fail_branch<F: Fn(GuardianStatus, Option<&str>)>(
    store: &Arc<Mutex<Store>>,
    id: &str,
    position: i64,
    branch: &str,
    err: &str,
    set_status: &F,
) {
    let _ = store.lock().expect("poisoned").set_branch_status(
        id,
        position,
        MergeStatus::Failed,
        Some(err),
    );
    set_status(
        GuardianStatus::MergeFailed,
        Some(&format!("branch {branch}: {err}")),
    );
}

/// The result of cherry-picking a branch's commits.
pub(crate) enum PickOutcome {
    /// Commits were applied.
    Applied,
    /// The branch had no commits beyond the base (nothing to do).
    Empty,
}

/// Cherry-pick `base..branch` into the current worktree. Returns an error string
/// (typically a conflict) on failure.
pub(crate) fn cherry_pick_branch(
    wt: &Path,
    base: &str,
    branch: &str,
) -> std::result::Result<PickOutcome, String> {
    let range = format!("{base}..{branch}");
    let count: i64 = git(wt, &["rev-list", "--count", &range])?
        .trim()
        .parse()
        .unwrap_or(0);
    if count == 0 {
        return Ok(PickOutcome::Empty);
    }
    git(wt, &["cherry-pick", &range])?;
    Ok(PickOutcome::Applied)
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
