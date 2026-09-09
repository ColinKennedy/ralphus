//! Real-git integration tests for submit-time review derivation: a worktree on a
//! feature branch becomes one guardian per project, with the branch collected and
//! the run association recorded. `<<upstream>>` without an upstream is rejected.

mod common;

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use common::{git, init_repo};
use ralphus_core::schema::TaskFile;
use ralphus_daemon::cancel::{CancelToken, Cancellations};
use ralphus_daemon::guardian_merge::{run_merge, start_merge};
use ralphus_daemon::reviews::derive_reviews;
use ralphus_daemon::runner::{Runner, RunnerResult, RunnerSpec, SubprocessRunner};
use ralphus_daemon::scheduler::{Semaphore, execute_squad, execute_squad_with};
use ralphus_daemon::store::{NodeState, SquadState, Store};

/// A runner that reports every session done without touching disk.
struct OkRunner;
impl Runner for OkRunner {
    fn run(&self, _spec: &RunnerSpec) -> RunnerResult {
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
            summary: "ok".to_string(),
            error: None,
            proofed: None,
            agent_session_id: None,
            ghost: None,
        }
    }
}

/// A runner that actually resolves git conflict markers in the worktree by
/// keeping the "ours" (HEAD) side of every conflict. Used to test the
/// conflict-resolution loop without requiring a live ollama instance.
struct ConflictResolvingRunner;

fn strip_conflict_markers(content: &str) -> String {
    let mut out = String::new();
    // 0 = normal, 1 = ours (keep), 2 = theirs (drop)
    let mut state: u8 = 0;
    for line in content.lines() {
        if line.starts_with("<<<<<<<") {
            state = 1;
        } else if line.starts_with("=======") && state == 1 {
            state = 2;
        } else if line.starts_with(">>>>>>>") && state == 2 {
            state = 0;
        } else if state != 2 {
            out.push_str(line);
            out.push('\n');
        }
    }
    out
}

impl Runner for ConflictResolvingRunner {
    fn run(&self, spec: &RunnerSpec) -> RunnerResult {
        let cwd = Path::new(&spec.cwd);
        let raw = Command::new("git")
            .args(["diff", "--name-only", "--diff-filter=U"])
            .current_dir(cwd)
            .output()
            .expect("git diff --diff-filter=U");
        for name in String::from_utf8_lossy(&raw.stdout).lines() {
            let path = cwd.join(name.trim());
            if let Ok(content) = std::fs::read_to_string(&path) {
                let _ = std::fs::write(&path, strip_conflict_markers(&content));
            }
        }
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
            summary: "conflicts resolved".to_string(),
            error: None,
            proofed: None,
            agent_session_id: None,
            ghost: None,
        }
    }
}

fn temp_base(tag: &str) -> PathBuf {
    static N: AtomicU32 = AtomicU32::new(0);
    let n = N.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!("ralphus-rev-{tag}-{}-{n}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("mkdir");
    dir
}

/// A repo with one commit on `main` and a linked worktree on `branch`. Returns
/// the base dir (to clean up) and the worktree path (forward-slashed for TOML).
fn repo_with_worktree(base: &Path, branch: &str) -> String {
    let repo = base.join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    init_repo(&repo);
    std::fs::write(repo.join("base.txt"), "base\n").unwrap();
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-m", "base"]);
    let wt = base.join(format!("wt-{}", branch.replace('/', "_")));
    git(
        &repo,
        &["worktree", "add", "-b", branch, wt.to_str().unwrap()],
    );
    git(&wt, &["branch", "--set-upstream-to=main"]);
    wt.to_string_lossy().replace('\\', "/")
}

/// Like `repo_with_worktree` but does NOT set an upstream tracking branch.
/// Used for tests that verify the "no upstream" error path.
fn repo_with_worktree_no_upstream(base: &Path, branch: &str) -> String {
    let repo = base.join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    init_repo(&repo);
    std::fs::write(repo.join("base.txt"), "base\n").unwrap();
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-m", "base"]);
    let wt = base.join(format!("wt-{}", branch.replace('/', "_")));
    git(
        &repo,
        &["worktree", "add", "-b", branch, wt.to_str().unwrap()],
    );
    wt.to_string_lossy().replace('\\', "/")
}

/// Commit a real change on a feature worktree returned by
/// [`repo_with_worktree`], so its branch actually contributes something to a
/// review stack.
///
/// `repo_with_worktree` branches straight off `main` and commits nothing, and
/// the runners used here (`OkRunner`/`NoopRunner`) never commit either — so
/// such a branch adds no diff over the branch beneath it, which a real merge
/// fails outright (RAL-190, `note_if_branch_is_empty`). Any test that runs a
/// merge and is *not* itself about that check must commit here first, or it
/// fails on emptiness long before reaching whatever it means to assert.
fn commit_on_worktree(cwd: &str, file: &str, contents: &str) {
    let native = cwd.replace('/', std::path::MAIN_SEPARATOR_STR);
    let wt = Path::new(&native);
    std::fs::write(wt.join(file), contents).unwrap();
    git(wt, &["add", "."]);
    git(wt, &["commit", "-m", &format!("add {file}")]);
}

/// The `<<...>>` sentinel form (RAL-269) of a cell's `review` field for the
/// given `[[review]].id` value: `<<ralphus:new-review/<key>>>` for a
/// new-review placeholder id, `<<review:<id>>>` for a plain existing id.
fn cell_review_sentinel(review_id: &str) -> String {
    if review_id.starts_with("ralphus:") {
        format!("<<{review_id}>>")
    } else {
        format!("<<review:{review_id}>>")
    }
}

/// Build a minimal task TOML with one session that opts into a review.
/// `review_id` is the id for both the session's `review` field (wrapped as a
/// `<<...>>` sentinel) and the top-level `[[review]]` block (unwrapped).
/// `review_attrs` is any extra `key = "value"` lines to append inside the
/// `[[review]]` block (may be empty).
fn session_toml(cwd: &str, review_id: &str, review_attrs: &str) -> String {
    let sentinel = cell_review_sentinel(review_id);
    format!(
        "[[task]]\nname=\"t\"\n[[task.cell]]\ncwd=\"{cwd}\"\nprompt=\"p\"\nreview=\"{sentinel}\"\n\
         [[review]]\nid=\"{review_id}\"\n{review_attrs}\n"
    )
}

/// Like [`session_toml`] but with TWO sessions in ONE submission, each in its own
/// worktree, both opting into the same review. This is how a multi-project (or
/// multi-branch) link-group guardian is built now that `ralphus:new-review/<key>`
/// only groups WITHIN a submission.
fn two_session_toml(cwd_a: &str, cwd_b: &str, review_id: &str, review_attrs: &str) -> String {
    let sentinel = cell_review_sentinel(review_id);
    format!(
        "[[task]]\nname=\"t\"\n\
         [[task.cell]]\ncwd=\"{cwd_a}\"\nprompt=\"p\"\nreview=\"{sentinel}\"\n\
         [[task.cell]]\ncwd=\"{cwd_b}\"\nprompt=\"p\"\nreview=\"{sentinel}\"\n\
         [[review]]\nid=\"{review_id}\"\n{review_attrs}\n"
    )
}

#[test]
fn single_project_makes_one_review() {
    let base = temp_base("single");
    let cwd = repo_with_worktree(&base, "feature/a");
    let toml = session_toml(
        &cwd,
        "backend",
        "agent=\"claude\"\nmodel=\"claude-opus-4-8\"\nproof_scope=\"each_branch\"\nskip_worktrees=true\nauto_pr_feedback=true\nskip_base_updates=true\nskip_auto_clean=true\nmatch_pr_branch_name=true\nseparate_pr_branch=true\nskip_auto_build=true",
    );
    let file: TaskFile = toml::from_str(&toml).unwrap();

    let mut store = Store::open_in_memory().unwrap();
    let run_id = store.insert_squad(&file, None, false).unwrap();
    let ids = derive_reviews(&store, &run_id, &file).expect("derive ok");

    assert_eq!(ids.len(), 1);
    let g = store.get_guardian(&ids[0]).unwrap();
    assert_eq!(g.name, "backend");
    assert_eq!(g.base_branch, "main");
    assert_eq!(g.squad_id.as_deref(), Some(run_id.as_str()));
    // The review's declared conflict-resolver backend/model is persisted.
    assert_eq!(g.resolver_agent.as_deref(), Some("claude"));
    assert_eq!(g.resolver_model.as_deref(), Some("claude-opus-4-8"));
    assert!(g.skip_worktrees);
    assert!(g.auto_pr_feedback);
    assert_eq!(g.skip_base_updates, Some(true));
    assert_eq!(g.proof_skip_auto_clean, Some(true));
    assert_eq!(g.match_pr_branch_name, Some(true));
    assert_eq!(g.separate_pr_branch, Some(true));
    assert_eq!(g.branches.len(), 1);
    assert_eq!(g.branches[0].branch, "feature/a");
    assert_eq!(store.guardians_for_squad(&run_id).unwrap(), ids);

    let _ = std::fs::remove_dir_all(&base);
}

#[test]
fn declared_review_settings_override_project_defaults() {
    let base = temp_base("review-settings-precedence");
    let cwd = repo_with_worktree(&base, "feature/settings");
    std::fs::write(
        std::path::Path::new(&cwd).join(".ralphus.toml"),
        "[review]\nskip_worktrees=false\nskip_base_updates=false\nmatch_pr_branch_name=false\nseparate_pr_branch=false\n",
    )
    .unwrap();
    let toml = session_toml(
        &cwd,
        "settings",
        "proof_scope=\"each_branch\"\nskip_worktrees=true\nskip_base_updates=true\nmatch_pr_branch_name=true\nseparate_pr_branch=true\nskip_auto_build=true",
    );
    let file: TaskFile = toml::from_str(&toml).unwrap();
    let mut store = Store::open_in_memory().unwrap();
    let squad_id = store.insert_squad(&file, None, false).unwrap();
    let guardian_id = derive_reviews(&store, &squad_id, &file).unwrap().remove(0);
    let guardian = store.get_guardian(&guardian_id).unwrap();

    assert!(guardian.skip_worktrees);
    assert_eq!(guardian.skip_base_updates, Some(true));
    assert_eq!(guardian.match_pr_branch_name, Some(true));
    assert_eq!(guardian.separate_pr_branch, Some(true));
    let _ = std::fs::remove_dir_all(&base);
}

#[test]
fn two_projects_make_two_disambiguated_reviews() {
    let base_a = temp_base("projA");
    let base_b = temp_base("projB");
    let cwd_a = repo_with_worktree(&base_a, "feature/a");
    let cwd_b = repo_with_worktree(&base_b, "feature/b");
    let toml = format!(
        "[[task]]\nname=\"t\"\n\
         [[task.cell]]\ncwd=\"{cwd_a}\"\nprompt=\"p\"\nreview=\"<<review:one>>\"\n\
         [[task.cell]]\ncwd=\"{cwd_b}\"\nprompt=\"p\"\nreview=\"<<review:two>>\"\n\
         [[review]]\nid=\"one\"\nskip_auto_build=true\n\
         [[review]]\nid=\"two\"\nskip_auto_build=true\n"
    );
    let file: TaskFile = toml::from_str(&toml).unwrap();

    let mut store = Store::open_in_memory().unwrap();
    let run_id = store.insert_squad(&file, None, false).unwrap();
    let ids = derive_reviews(&store, &run_id, &file).expect("derive ok");

    assert_eq!(ids.len(), 2, "two projects -> two reviews");
    let names: Vec<String> = ids
        .iter()
        .map(|id| store.get_guardian(id).unwrap().name)
        .collect();
    // Disambiguated with numeric suffixes; the two names must differ.
    assert_ne!(names[0], names[1]);
    assert!(names.iter().all(|n| n.contains("-0")), "names: {names:?}");

    let _ = std::fs::remove_dir_all(&base_a);
    let _ = std::fs::remove_dir_all(&base_b);
}

/// One repo with two linked worktrees on `branch_a` / `branch_b`. Returns the
/// repo base plus each worktree's forward-slashed path (for TOML `cwd`).
fn repo_with_two_worktrees(base: &Path, branch_a: &str, branch_b: &str) -> (String, String) {
    let repo = base.join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    init_repo(&repo);
    std::fs::write(repo.join("base.txt"), "base\n").unwrap();
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-m", "base"]);
    let wta = base.join("wt-a");
    let wtb = base.join("wt-b");
    git(
        &repo,
        &["worktree", "add", "-b", branch_a, wta.to_str().unwrap()],
    );
    git(
        &repo,
        &["worktree", "add", "-b", branch_b, wtb.to_str().unwrap()],
    );
    git(&wta, &["branch", "--set-upstream-to=main"]);
    git(&wtb, &["branch", "--set-upstream-to=main"]);
    (
        wta.to_string_lossy().replace('\\', "/"),
        wtb.to_string_lossy().replace('\\', "/"),
    )
}

/// RAL-314: repeat submissions against the SAME worktree (no branch switch)
/// each mint their own fresh guardian (`separate_submissions_each_mint_a_fresh_review`
/// above already proves that write-side fact), but both guardians record the
/// identical branch *string* -- before this fix, the read-side branch-string
/// join then conflated them, so squad B's cell showed up as "in" squad A's
/// review and vice versa (and the scheduler's `collecting_guardians_for_cells`
/// readiness lookup made the same mistake). Each squad's cell must resolve
/// only to the guardian its OWN submission created.
#[test]
fn repeat_submission_against_the_same_worktree_does_not_conflate_reviews() {
    let base = temp_base("collision");
    let cwd = repo_with_worktree(&base, "feature/shared");
    let mut store = Store::open_in_memory().unwrap();

    let toml_a = session_toml(
        &cwd,
        "ralphus:new-review/batch",
        "name=\"Batch\"\nskip_auto_build=true",
    );
    let file_a: TaskFile = toml::from_str(&toml_a).unwrap();
    let squad_a = store.insert_squad(&file_a, None, false).unwrap();
    let ids_a = derive_reviews(&store, &squad_a, &file_a).expect("derive a");
    assert_eq!(ids_a.len(), 1);
    let gid_a = ids_a[0].clone();

    // Second submission, same worktree/branch, no branch switch in between --
    // it mints its OWN guardian (per `separate_submissions_each_mint_a_fresh_review`)
    // that happens to record the exact same branch string as guardian A.
    let toml_b = session_toml(
        &cwd,
        "ralphus:new-review/batch",
        "name=\"Batch\"\nskip_auto_build=true",
    );
    let file_b: TaskFile = toml::from_str(&toml_b).unwrap();
    let squad_b = store.insert_squad(&file_b, None, false).unwrap();
    let ids_b = derive_reviews(&store, &squad_b, &file_b).expect("derive b");
    assert_eq!(ids_b.len(), 1);
    let gid_b = ids_b[0].clone();

    assert_ne!(gid_a, gid_b, "each submission mints its own guardian");
    assert_eq!(
        store.get_guardian(&gid_a).unwrap().branches[0].branch,
        store.get_guardian(&gid_b).unwrap().branches[0].branch,
        "sanity: both guardians recorded the identical branch string"
    );

    // The cell-level "in reviews" list (RAL-17, via `get_squad`) must link
    // each squad's cell to only its own guardian.
    let view_a = store.get_squad(&squad_a).unwrap();
    let cell_a = &view_a.tasks[0].cells[0];
    assert_eq!(
        cell_a
            .reviews
            .iter()
            .map(|r| r.id.clone())
            .collect::<Vec<_>>(),
        vec![gid_a.clone()],
        "squad A's cell must link only to guardian A, not guardian B"
    );

    let view_b = store.get_squad(&squad_b).unwrap();
    let cell_b = &view_b.tasks[0].cells[0];
    assert_eq!(
        cell_b
            .reviews
            .iter()
            .map(|r| r.id.clone())
            .collect::<Vec<_>>(),
        vec![gid_b.clone()],
        "squad B's cell must link only to guardian B, not guardian A"
    );

    // The scheduler's stack-readiness gating (`collecting_guardians_for_cells`)
    // must draw the same distinction, not just the cosmetic board view.
    assert_eq!(
        store.collecting_guardians_for_cells(&squad_a).unwrap(),
        vec![gid_a],
        "squad A must only be seen as contributing to guardian A"
    );
    assert_eq!(
        store.collecting_guardians_for_cells(&squad_b).unwrap(),
        vec![gid_b],
        "squad B must only be seen as contributing to guardian B"
    );

    let _ = std::fs::remove_dir_all(&base);
}

#[test]
fn separate_submissions_each_mint_a_fresh_review() {
    // `ralphus:new-review/<key>` is a submission-LOCAL placeholder: it always mints
    // a brand-new guardian for the submission that uses it. Two separate
    // submissions that happen to reuse the same <key> string get two independent
    // guardians — the placeholder never links across submissions.
    let base = temp_base("link");
    let (cwd_a, cwd_b) = repo_with_two_worktrees(&base, "feature/a", "feature/b");
    let mut store = Store::open_in_memory().unwrap();

    // Submission 1 (branch a) mints a fresh guardian, tagged with run 1.
    let file1: TaskFile = toml::from_str(&session_toml(
        &cwd_a,
        "ralphus:new-review/batch",
        "name=\"My Batch\"\nskip_auto_build=true",
    ))
    .unwrap();
    let run1 = store.insert_squad(&file1, None, false).unwrap();
    let ids1 = derive_reviews(&store, &run1, &file1).expect("derive 1");
    assert_eq!(ids1.len(), 1, "first submission mints one guardian");
    let gid1 = ids1[0].clone();
    assert_eq!(store.get_guardian(&gid1).unwrap().name, "My Batch");

    // Submission 2 (branch b) reuses the same <key> string but is a SEPARATE
    // submission, so it mints its OWN new guardian — it does not attach to gid1.
    let file2: TaskFile = toml::from_str(&session_toml(
        &cwd_b,
        "ralphus:new-review/batch",
        "name=\"My Batch\"\nskip_auto_build=true",
    ))
    .unwrap();
    let run2 = store.insert_squad(&file2, None, false).unwrap();
    let ids2 = derive_reviews(&store, &run2, &file2).expect("derive 2");
    assert_eq!(
        ids2.len(),
        1,
        "second submission mints its own new guardian"
    );
    let gid2 = ids2[0].clone();
    assert_ne!(gid1, gid2, "the two submissions must not share a guardian");
    assert_eq!(
        store.guardians_for_squad(&run2).unwrap(),
        vec![gid2.clone()],
        "the new guardian is tagged with the second run"
    );

    // Each guardian carries only its own submission's branch.
    let b1: Vec<String> = store
        .get_guardian(&gid1)
        .unwrap()
        .branches
        .iter()
        .map(|b| b.branch.clone())
        .collect();
    let b2: Vec<String> = store
        .get_guardian(&gid2)
        .unwrap()
        .branches
        .iter()
        .map(|b| b.branch.clone())
        .collect();
    assert_eq!(b1, vec!["feature/a"]);
    assert_eq!(b2, vec!["feature/b"]);

    let _ = std::fs::remove_dir_all(&base);
}

/// One repo with three linked worktrees on `branch_a` / `branch_b` / `branch_c`.
/// Returns the repo base plus each worktree's forward-slashed path.
fn repo_with_three_worktrees(
    base: &Path,
    branch_a: &str,
    branch_b: &str,
    branch_c: &str,
) -> (String, String, String) {
    let repo = base.join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    init_repo(&repo);
    std::fs::write(repo.join("base.txt"), "base\n").unwrap();
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-m", "base"]);
    let wta = base.join("wt-a");
    let wtb = base.join("wt-b");
    let wtc = base.join("wt-c");
    git(
        &repo,
        &["worktree", "add", "-b", branch_a, wta.to_str().unwrap()],
    );
    git(
        &repo,
        &["worktree", "add", "-b", branch_b, wtb.to_str().unwrap()],
    );
    git(
        &repo,
        &["worktree", "add", "-b", branch_c, wtc.to_str().unwrap()],
    );
    git(&wta, &["branch", "--set-upstream-to=main"]);
    git(&wtb, &["branch", "--set-upstream-to=main"]);
    git(&wtc, &["branch", "--set-upstream-to=main"]);
    (
        wta.to_string_lossy().replace('\\', "/"),
        wtb.to_string_lossy().replace('\\', "/"),
        wtc.to_string_lossy().replace('\\', "/"),
    )
}

/// Mirrors what `ralphus submit a.toml b.toml c.toml` now does client-side:
/// read each file's TOML text independently and join them with a blank line
/// into ONE combined submission before parsing/deriving reviews. Three
/// self-contained "files" (each with its own `[[task]]` and its own
/// `[[review]]` block, per the tutor's documented convention) are combined:
/// two name the same `ralphus:new-review/<key>` and one names a different key.
/// Combined in ONE submission, this must produce exactly two guardians — the
/// shared-key one with two branches, the solo-key one with one branch.
#[test]
fn three_files_combined_into_one_submission_two_keys_make_two_reviews() {
    let base = temp_base("threefiles");
    let (cwd_a, cwd_b, cwd_c) =
        repo_with_three_worktrees(&base, "feature/a", "feature/b", "feature/c");

    // File 1: task "t1", opts into the shared key.
    let file1_text = format!(
        "[[task]]\nname=\"t1\"\n\
         [[task.cell]]\ncwd=\"{cwd_a}\"\nprompt=\"p\"\nreview=\"<<ralphus:new-review/shared>>\"\n\
         [[review]]\nid=\"ralphus:new-review/shared\"\nname=\"Shared Batch\"\nskip_auto_build=true\n"
    );
    // File 2: task "t2", opts into the SAME shared key (repeated [[review]]
    // block, matching the documented multi-file convention).
    let file2_text = format!(
        "[[task]]\nname=\"t2\"\n\
         [[task.cell]]\ncwd=\"{cwd_b}\"\nprompt=\"p\"\nreview=\"<<ralphus:new-review/shared>>\"\n\
         [[review]]\nid=\"ralphus:new-review/shared\"\nname=\"Shared Batch\"\nskip_auto_build=true\n"
    );
    // File 3: task "t3", opts into a DIFFERENT key -- must end up in its own review.
    let file3_text = format!(
        "[[task]]\nname=\"t3\"\n\
         [[task.cell]]\ncwd=\"{cwd_c}\"\nprompt=\"p\"\nreview=\"<<ralphus:new-review/solo>>\"\n\
         [[review]]\nid=\"ralphus:new-review/solo\"\nname=\"Solo\"\nskip_auto_build=true\n"
    );

    // Exactly what `_cmd_submit` does for multiple file args: join with a blank
    // line, submit once.
    let combined = format!("{file1_text}\n\n{file2_text}\n\n{file3_text}");
    let file: TaskFile = toml::from_str(&combined).expect("combined TOML parses");

    let mut store = Store::open_in_memory().unwrap();
    let run_id = store.insert_squad(&file, None, false).unwrap();
    let ids = derive_reviews(&store, &run_id, &file).expect("derive ok");

    assert_eq!(
        ids.len(),
        2,
        "two distinct keys in one submission -> exactly two reviews"
    );

    let guardians: Vec<_> = ids
        .iter()
        .map(|id| store.get_guardian(id).unwrap())
        .collect();
    let shared = guardians
        .iter()
        .find(|g| g.name == "Shared Batch")
        .expect("a 'Shared Batch' guardian exists");
    let solo = guardians
        .iter()
        .find(|g| g.name == "Solo")
        .expect("a 'Solo' guardian exists");

    let shared_branches: Vec<String> = shared.branches.iter().map(|b| b.branch.clone()).collect();
    let solo_branches: Vec<String> = solo.branches.iter().map(|b| b.branch.clone()).collect();

    assert_eq!(
        shared_branches.len(),
        2,
        "the shared-key review collects both branches: {shared_branches:?}"
    );
    assert!(shared_branches.contains(&"feature/a".to_string()));
    assert!(shared_branches.contains(&"feature/b".to_string()));
    assert_eq!(solo_branches, vec!["feature/c".to_string()]);

    let _ = std::fs::remove_dir_all(&base);
}

#[test]
fn upstream_base_without_upstream_is_rejected() {
    let base = temp_base("noup");
    let cwd = repo_with_worktree_no_upstream(&base, "feature/a");
    let toml = session_toml(&cwd, "r", "");
    let file: TaskFile = toml::from_str(&toml).unwrap();

    let mut store = Store::open_in_memory().unwrap();
    let run_id = store.insert_squad(&file, None, false).unwrap();
    let err = derive_reviews(&store, &run_id, &file).expect_err("no upstream -> error");
    assert!(err.message.contains("upstream"), "msg: {}", err.message);

    let _ = std::fs::remove_dir_all(&base);
}

#[test]
fn reviews_auto_start_when_the_run_succeeds() {
    let base = temp_base("autostart");
    // Deliberately left with NO commits of its own. Unlike the other merge
    // tests here, this one's merge is auto-started by the *scheduler*, which
    // builds its own real runner rather than taking `&OkRunner` -- so a branch
    // with real work would drive the rebase on into the resolver/final-proof
    // machinery and call a live Ollama model, which this suite must never do
    // by default (see the Ollama opt-in rule in AGENTS.md). An empty branch
    // terminates the auto-started merge immediately and hermetically.
    let cwd = repo_with_worktree(&base, "feature/a");
    let toml = format!(
        "[[task]]\nname=\"t\"\n[[task.cell]]\ncwd=\"{cwd}\"\ncommand=\"noop\"\nreview=\"<<review:r>>\"\n\
         [[review]]\nid=\"r\"\nskip_auto_build=true\n"
    );
    let file: TaskFile = toml::from_str(&toml).unwrap();

    let store = Arc::new(Mutex::new(Store::open_in_memory().unwrap()));
    let (run_id, gid) = {
        let mut g = store.lock().unwrap();
        let run_id = g.insert_squad(&file, None, false).unwrap();
        let ids = derive_reviews(&g, &run_id, &file).expect("derive");
        assert_eq!(g.get_guardian(&ids[0]).unwrap().status, "collecting");
        (run_id, ids[0].clone())
    };

    // Running the run to success should auto-start the review merge.
    execute_squad(&store, &OkRunner, &run_id);
    assert_eq!(
        store.lock().unwrap().squad_state(&run_id).unwrap(),
        SquadState::Done
    );

    // The merge runs on a spawned thread; poll until it reaches review. Bounded
    // generously (30s) since this does real git subprocess work and can be
    // slow under CPU contention when the full suite runs many tests in parallel.
    let mut status = String::new();
    for _ in 0..2000 {
        status = store.lock().unwrap().get_guardian(&gid).unwrap().status;
        if status == "in_review" || status == "merge_failed" {
            break;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    // What is under test is that the merge started *by itself* when the run
    // finished -- not its verdict. Depending on whether a resolver is
    // available, this hermetic fixture fails either during resolver preflight
    // or when RAL-190 observes the deliberately empty branch. Reaching
    // `merge_failed` (rather than sitting in `collecting` forever) is proof
    // the auto-start fired.
    assert_eq!(
        status, "merge_failed",
        "review should auto-start when the run succeeds; its lone branch is \
         empty and the merge it starts should reach a terminal failure"
    );

    let _ = std::fs::remove_dir_all(&base);
}

// ── regression: Merge/rebase button path with conflict resolution ─────────────

/// The "Merge / rebase" button path (start_merge → run_merge →
/// resolve_conflicts_with_agent) must invoke the agent and produce a
/// conflict-free tree, not abort on the first unresolved pass.
#[test]
fn start_merge_resolves_conflicts_with_agent() {
    let base = temp_base("startmerge");
    let (cwd_a, cwd_b) = two_conflicting_worktrees(&base);
    let toml = format!(
        "[[task]]\nname=\"a\"\n\
         [[task.cell]]\ncwd=\"{cwd_a}\"\ncommand=\"noop\"\nreview=\"<<review:rev>>\"\n\
         [[task]]\nname=\"b\"\ndepends_on=[\"a\"]\n\
         [[task.cell]]\ncwd=\"{cwd_b}\"\ncommand=\"noop\"\nreview=\"<<review:rev>>\"\n\
         [[review]]\nid=\"rev\"\nskip_auto_build=true\n"
    );
    let file: TaskFile = toml::from_str(&toml).unwrap();
    let store = Arc::new(Mutex::new(Store::open_in_memory().unwrap()));
    let run_id = store
        .lock()
        .unwrap()
        .insert_squad(&file, None, false)
        .unwrap();
    let gid = {
        let g = store.lock().unwrap();
        derive_reviews(&g, &run_id, &file).expect("derive")[0].clone()
    };
    // RAL-255: start_merge now defers a still-collecting guardian's merge
    // while an enabled branch's upstream Cell isn't done yet, so mark both
    // cells done first -- matching the real precondition for the "Merge /
    // rebase" button to actually be pressable.
    {
        let g = store.lock().unwrap();
        g.set_cell_state(&run_id, 0, 0, NodeState::Done).unwrap();
        g.set_cell_state(&run_id, 1, 0, NodeState::Done).unwrap();
    }

    // Simulate the "Merge / rebase" button.
    let runner = Arc::new(ConflictResolvingRunner) as Arc<dyn Runner>;
    let _ = start_merge(
        Arc::clone(&store),
        runner,
        &gid,
        Arc::new(Semaphore::new(4)),
        Cancellations::new(),
    );

    // Generous poll budget: under full-suite parallel load (many git worktree
    // ops + concurrent tests contending for CPU), this can take much longer
    // than it does in isolation even though no live network call is involved.
    let mut status = String::new();
    for _ in 0..2400 {
        status = store.lock().unwrap().get_guardian(&gid).unwrap().status;
        if status == "in_review" || status == "merge_failed" {
            break;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    assert_eq!(
        status, "in_review",
        "conflict must be resolved and guardian reach in_review"
    );

    let view = store.lock().unwrap().get_guardian(&gid).unwrap();
    let combined = view
        .combined_worktree
        .expect("combined worktree must exist");
    let content = std::fs::read_to_string(Path::new(&combined).join("shared.txt")).unwrap();
    assert!(
        !content.contains("<<<<<<<"),
        "conflict markers must not remain: {content}"
    );
    assert!(
        view.branches
            .iter()
            .any(|b| b.merge_status == "conflict_resolved"),
        "at least one branch must report conflict_resolved: {:?}",
        view.branches
            .iter()
            .map(|b| (&b.branch, &b.merge_status))
            .collect::<Vec<_>>()
    );

    let _ = std::fs::remove_dir_all(&base);
}

/// After a feature branch is updated (simulating a force-push that introduces
/// a new conflict), pressing "Merge / rebase" must resolve the conflict and
/// reach in_review — not abort.
#[test]
fn force_push_then_merge_resolves_cleanly() {
    let base = temp_base("forcepush");

    // Initial repo: A edits shared.txt; B edits b_only.txt (no conflict yet).
    let repo = base.join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    init_repo(&repo);
    std::fs::write(repo.join("shared.txt"), "original\n").unwrap();
    std::fs::write(repo.join("b_only.txt"), "b_only\n").unwrap();
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-m", "base"]);

    let wt_a = base.join("wt-a");
    let wt_b = base.join("wt-b");
    git(
        &repo,
        &["worktree", "add", "-b", "feature/a", wt_a.to_str().unwrap()],
    );
    git(
        &repo,
        &["worktree", "add", "-b", "feature/b", wt_b.to_str().unwrap()],
    );
    git(&wt_a, &["branch", "--set-upstream-to=main"]);
    git(&wt_b, &["branch", "--set-upstream-to=main"]);
    std::fs::write(wt_a.join("shared.txt"), "changed by A\n").unwrap();
    git(&wt_a, &["commit", "-am", "A edits shared.txt"]);
    std::fs::write(wt_b.join("b_only.txt"), "changed by B\n").unwrap();
    git(&wt_b, &["commit", "-am", "B edits b_only.txt"]);

    let cwd_a = wt_a.to_string_lossy().replace('\\', "/");
    let cwd_b = wt_b.to_string_lossy().replace('\\', "/");
    let toml = format!(
        "[[task]]\nname=\"a\"\n\
         [[task.cell]]\ncwd=\"{cwd_a}\"\ncommand=\"noop\"\nreview=\"<<review:rev>>\"\n\
         [[task]]\nname=\"b\"\ndepends_on=[\"a\"]\n\
         [[task.cell]]\ncwd=\"{cwd_b}\"\ncommand=\"noop\"\nreview=\"<<review:rev>>\"\n\
         [[review]]\nid=\"rev\"\nskip_auto_build=true\n"
    );
    let file: TaskFile = toml::from_str(&toml).unwrap();
    let store = Arc::new(Mutex::new(Store::open_in_memory().unwrap()));
    let run_id = store
        .lock()
        .unwrap()
        .insert_squad(&file, None, false)
        .unwrap();
    let gid = {
        let g = store.lock().unwrap();
        derive_reviews(&g, &run_id, &file).expect("derive")[0].clone()
    };

    // First merge: A and B don't conflict, so OkRunner (no resolution needed).
    run_merge(&store, &OkRunner, &gid);
    assert_eq!(
        store.lock().unwrap().get_guardian(&gid).unwrap().status,
        "in_review",
        "initial clean merge should reach in_review"
    );

    // Force-push: B now also edits shared.txt, conflicting with A's review branch.
    std::fs::write(wt_b.join("shared.txt"), "changed by B (force-pushed)\n").unwrap();
    git(
        &wt_b,
        &[
            "commit",
            "-am",
            "B force-pushes conflicting change to shared.txt",
        ],
    );

    // Press "Merge / rebase" after the force-push.
    let runner = Arc::new(ConflictResolvingRunner) as Arc<dyn Runner>;
    let _ = start_merge(
        Arc::clone(&store),
        runner,
        &gid,
        Arc::new(Semaphore::new(4)),
        Cancellations::new(),
    );

    let mut status = String::new();
    for _ in 0..2000 {
        status = store.lock().unwrap().get_guardian(&gid).unwrap().status;
        if status == "in_review" || status == "merge_failed" {
            break;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    assert_eq!(
        status, "in_review",
        "after force-push conflict, Merge/rebase must resolve cleanly"
    );

    let view = store.lock().unwrap().get_guardian(&gid).unwrap();
    let combined = view
        .combined_worktree
        .expect("combined worktree must exist");
    let content = std::fs::read_to_string(Path::new(&combined).join("shared.txt")).unwrap();
    assert!(
        !content.contains("<<<<<<<"),
        "conflict markers must not remain after force-push re-merge: {content}"
    );

    let _ = std::fs::remove_dir_all(&base);
}

/// RAL-108: pressing "Merge / rebase" a second time on a review that is
/// already `in_review` (branches merged and marked done, nothing changed
/// since) must force a fresh rebase rather than silently 409ing. `start_merge`
/// is the exact function the HTTP `/merge` endpoint calls, so exercising it
/// directly proves the button's backend path actually re-runs the full
/// stacked-rebase walk instead of no-oping on `in_review`.
#[test]
fn merge_button_forces_a_fresh_rebase_on_an_already_in_review_review() {
    let base = temp_base("rebutton");
    let repo = base.join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    init_repo(&repo);
    std::fs::write(repo.join("base.txt"), "base\n").unwrap();
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-m", "base"]);

    let wt_a = base.join("wt-a");
    let wt_b = base.join("wt-b");
    git(
        &repo,
        &["worktree", "add", "-b", "feature/a", wt_a.to_str().unwrap()],
    );
    git(
        &repo,
        &["worktree", "add", "-b", "feature/b", wt_b.to_str().unwrap()],
    );
    git(&wt_a, &["branch", "--set-upstream-to=main"]);
    git(&wt_b, &["branch", "--set-upstream-to=main"]);
    std::fs::write(wt_a.join("a.txt"), "from a\n").unwrap();
    git(&wt_a, &["add", "."]);
    git(&wt_a, &["commit", "-m", "add a"]);
    std::fs::write(wt_b.join("b.txt"), "from b\n").unwrap();
    git(&wt_b, &["add", "."]);
    git(&wt_b, &["commit", "-m", "add b"]);

    let cwd_a = wt_a.to_string_lossy().replace('\\', "/");
    let cwd_b = wt_b.to_string_lossy().replace('\\', "/");
    let toml = format!(
        "[[task]]\nname=\"a\"\n\
         [[task.cell]]\ncwd=\"{cwd_a}\"\ncommand=\"noop\"\nreview=\"<<review:rev>>\"\n\
         [[task]]\nname=\"b\"\ndepends_on=[\"a\"]\n\
         [[task.cell]]\ncwd=\"{cwd_b}\"\ncommand=\"noop\"\nreview=\"<<review:rev>>\"\n\
         [[review]]\nid=\"rev\"\nskip_auto_build=true\n"
    );
    let file: TaskFile = toml::from_str(&toml).unwrap();
    let store = Arc::new(Mutex::new(Store::open_in_memory().unwrap()));
    let run_id = store
        .lock()
        .unwrap()
        .insert_squad(&file, None, false)
        .unwrap();
    let gid = {
        let g = store.lock().unwrap();
        derive_reviews(&g, &run_id, &file).expect("derive")[0].clone()
    };

    // First "Merge / rebase" press: clean stack, reaches in_review with both
    // branches done.
    run_merge(&store, &OkRunner, &gid);
    {
        let view = store.lock().unwrap().get_guardian(&gid).unwrap();
        assert_eq!(view.status, "in_review");
        assert!(view.branches.iter().all(|b| b.merge_status == "done"));
    }

    // Press "Merge / rebase" again on the now-`in_review` review — nothing
    // about the branches changed, so before RAL-108 this 409'd as a no-op
    // instead of forcing a fresh rebase.
    let reply = start_merge(
        Arc::clone(&store),
        Arc::new(OkRunner),
        &gid,
        Arc::new(Semaphore::new(4)),
        Cancellations::new(),
    );
    assert_eq!(
        reply.status, 202,
        "the forced re-rebase must be accepted, not 409'd: {}",
        reply.body
    );

    // This test drives two full merge cycles (one synchronous, one via the
    // spawned background thread), so it needs more headroom than the other
    // single-merge polling loops in this file (which use 200 * 25ms = 5s).
    let mut status = String::new();
    for _ in 0..1200 {
        status = store.lock().unwrap().get_guardian(&gid).unwrap().status;
        if status == "in_review" || status == "merge_failed" {
            break;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    assert_eq!(
        status, "in_review",
        "forced rebase must complete and return to in_review"
    );
    let view = store.lock().unwrap().get_guardian(&gid).unwrap();
    assert!(
        view.branches.iter().all(|b| b.merge_status == "done"),
        "branches must be walked back through to done: {:?}",
        view.branches
            .iter()
            .map(|b| (&b.branch, &b.merge_status))
            .collect::<Vec<_>>()
    );

    let _ = std::fs::remove_dir_all(&base);
}

// ── RAL-101: project-level auto-build fallback ───────────────────────────────

/// A review with no explicit `checks` configured still gets a build/test
/// signal: the project's `.ralphus.toml` `[review] auto_build` default runs
/// once the stack finishes merging, and its outcome is recorded on the
/// guardian for the Reviews UI.
#[test]
fn no_checks_configured_runs_project_auto_build_default() {
    let base = temp_base("autobuild-default");
    let cwd = repo_with_worktree(&base, "feature/a");
    // This test is about the auto_build default, not about empty branches.
    commit_on_worktree(&cwd, "a.txt", "change by a\n");
    let repo = base.join("repo");
    std::fs::write(
        repo.join(".ralphus.toml"),
        "[review]\nauto_build = \"echo built > autobuild_ran.txt\"\n",
    )
    .unwrap();

    let toml = session_toml(&cwd, "rev", "");
    let file: TaskFile = toml::from_str(&toml).unwrap();
    let store = Arc::new(Mutex::new(Store::open_in_memory().unwrap()));
    let run_id = store
        .lock()
        .unwrap()
        .insert_squad(&file, None, false)
        .unwrap();
    let gid = {
        let g = store.lock().unwrap();
        derive_reviews(&g, &run_id, &file).expect("derive")[0].clone()
    };
    assert!(
        store
            .lock()
            .unwrap()
            .guardian_checks(&gid)
            .unwrap()
            .is_empty(),
        "sanity: this review has no explicit checks configured"
    );

    run_merge(&store, &OkRunner, &gid);

    let view = store.lock().unwrap().get_guardian(&gid).unwrap();
    assert_eq!(
        view.status, "in_review",
        "auto-build must pass and the review must reach in_review: detail={:?}",
        view.detail
    );
    let combined = view
        .combined_worktree
        .clone()
        .expect("combined worktree must exist");
    assert!(
        Path::new(&combined).join("autobuild_ran.txt").exists(),
        "the project's auto_build default must actually have run in the combined worktree"
    );
    assert_eq!(
        view.detail.as_deref(),
        Some("auto-built via project default: echo built > autobuild_ran.txt"),
        "the guardian detail must record that the project auto-build ran, for the Reviews UI"
    );

    let _ = std::fs::remove_dir_all(&base);
}

/// A review WITH explicit `checks` configured must not also run the project's
/// `auto_build` default — checks take priority and the review is not
/// double-built. Modeled by making the (unused) auto_build command always fail
/// while the explicit check passes: if auto-build ran too, the merge would
/// fail.
#[test]
fn checks_configured_does_not_also_run_auto_build() {
    let base = temp_base("autobuild-skip");
    let cwd = repo_with_worktree(&base, "feature/a");
    // This test is about explicit checks suppressing auto_build, not about
    // empty branches.
    commit_on_worktree(&cwd, "a.txt", "change by a\n");
    let repo = base.join("repo");
    std::fs::write(
        repo.join(".ralphus.toml"),
        "[review]\nauto_build = \"exit 1\"\n",
    )
    .unwrap();

    let toml = session_toml(&cwd, "rev", "");
    let file: TaskFile = toml::from_str(&toml).unwrap();
    let store = Arc::new(Mutex::new(Store::open_in_memory().unwrap()));
    let run_id = store
        .lock()
        .unwrap()
        .insert_squad(&file, None, false)
        .unwrap();
    let gid = {
        let g = store.lock().unwrap();
        derive_reviews(&g, &run_id, &file).expect("derive")[0].clone()
    };
    store
        .lock()
        .unwrap()
        .set_guardian_checks(&gid, &["exit 0".to_string()])
        .unwrap();

    run_merge(&store, &OkRunner, &gid);

    let view = store.lock().unwrap().get_guardian(&gid).unwrap();
    assert_eq!(
        view.status, "in_review",
        "explicit checks passed; the always-failing auto_build default must not have run \
         (that would have failed the merge): detail={:?}",
        view.detail
    );
    assert_ne!(
        view.detail.as_deref(),
        Some("auto-built via project default: exit 1"),
        "auto-build must not be recorded as having run when explicit checks are configured"
    );

    let _ = std::fs::remove_dir_all(&base);
}

// ── per-task review readiness (RAL-32) ───────────────────────────────────────

fn ok_result() -> RunnerResult {
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
        summary: "ok".to_string(),
        error: None,
        proofed: None,
        agent_session_id: None,
        ghost: None,
    }
}

/// A runner that succeeds immediately for sessions whose cwd starts with
/// `fast_prefix` and blocks (until cancelled or released) for all others.
struct GatableRunner {
    fast_prefix: String,
    started: Arc<AtomicBool>,
    released: Arc<AtomicBool>,
}

impl Runner for GatableRunner {
    fn run(&self, spec: &RunnerSpec) -> RunnerResult {
        self.run_cancellable(spec, &CancelToken::never())
    }

    fn run_cancellable(&self, spec: &RunnerSpec, cancel: &CancelToken) -> RunnerResult {
        if spec.cwd.starts_with(&self.fast_prefix) {
            return ok_result();
        }
        self.started.store(true, Ordering::SeqCst);
        while !self.released.load(Ordering::SeqCst) && !cancel.is_cancelled() {
            std::thread::sleep(Duration::from_millis(2));
        }
        if cancel.is_cancelled() {
            return RunnerResult {
                status: "failed".to_string(),
                tokens_in: 0,
                tokens_out: 0,
                cache_creation_tokens: 0,
                cache_read_tokens: 0,
                compaction_input_tokens: 0,
                compaction_count: 0,
                cost_usd: 0.0,
                cost_is_estimated: false,
                summary: String::new(),
                error: Some("cancelled".to_string()),
                proofed: None,
                agent_session_id: None,
                ghost: None,
            };
        }
        ok_result()
    }
}

/// Poll `check` up to `timeout`, sleeping 10 ms between attempts. Panics with
/// `msg` if the condition never becomes true.
fn poll_until(timeout: Duration, msg: &str, mut check: impl FnMut() -> bool) {
    let deadline = std::time::Instant::now() + timeout;
    while std::time::Instant::now() < deadline {
        if check() {
            return;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    panic!("{msg}");
}

/// A task whose session cwd is in a different project does not block a
/// guardian's readiness. The guardian for task A's project starts as soon as
/// task A is Done — without waiting for the unrelated blocking task B to finish.
#[test]
fn non_overlapping_task_does_not_block_readiness() {
    let base_x = temp_base("noblock-x");
    let base_y = temp_base("noblock-y");
    let cwd_a = repo_with_worktree(&base_x, "feature/a");
    let cwd_b = repo_with_worktree(&base_y, "feature/b");

    // Task A: fast, in project-X, declares a review.
    // Task B: slow/blocking, in project-Y, no review declaration.
    let toml = format!(
        "[[task]]\nname=\"a\"\n[[task.cell]]\ncwd=\"{cwd_a}\"\nprompt=\"p\"\nreview=\"<<review:r>>\"\n\
         [[task]]\nname=\"b\"\n[[task.cell]]\ncwd=\"{cwd_b}\"\nprompt=\"p\"\n\
         [[review]]\nid=\"r\"\nskip_auto_build=true\n"
    );
    let file: TaskFile = toml::from_str(&toml).unwrap();

    let store = Arc::new(Mutex::new(Store::open_in_memory().unwrap()));
    let (run_id, gid) = {
        let mut g = store.lock().unwrap();
        let run_id = g.insert_squad(&file, None, false).unwrap();
        let ids = derive_reviews(&g, &run_id, &file).expect("derive");
        assert_eq!(ids.len(), 1, "only task A declared a review");
        (run_id, ids[0].clone())
    };

    let started = Arc::new(AtomicBool::new(false));
    let released = Arc::new(AtomicBool::new(false));
    let runner: Arc<dyn Runner> = Arc::new(GatableRunner {
        fast_prefix: cwd_a.clone(),
        started: Arc::clone(&started),
        released: Arc::clone(&released),
    });
    let token = CancelToken::new();

    let store2 = Arc::clone(&store);
    let runner2 = Arc::clone(&runner);
    let run_id2 = run_id.clone();
    let token2 = token.clone();
    // RAL-213: a fresh, private registry -- this test only exercises the
    // task-completion-triggered review-start path, not a guardian-merge
    // restart via that registry.
    let handle = std::thread::spawn(move || {
        execute_squad_with(
            &store2,
            runner2.as_ref(),
            &run_id2,
            &token2,
            &Cancellations::new(),
        )
    });

    // Wait until task B's session is blocking (task A must have finished first).
    poll_until(Duration::from_secs(5), "task B should have started", || {
        started.load(Ordering::SeqCst)
    });

    // Give the per-task review start a moment to propagate through the
    // background thread that run_merge spawns.
    poll_until(
        Duration::from_secs(5),
        "guardian should start (not 'collecting') once task A is done",
        || store.lock().unwrap().get_guardian(&gid).unwrap().status != "collecting",
    );

    // Guardian started while task B is still blocking — confirm task B hasn't
    // finished yet (released is still false).
    assert!(
        !released.load(Ordering::SeqCst),
        "guardian should have started before task B was released"
    );

    // Clean up: release task B so execute_squad can finish.
    released.store(true, Ordering::SeqCst);
    handle.join().unwrap();

    let _ = std::fs::remove_dir_all(&base_x);
    let _ = std::fs::remove_dir_all(&base_y);
}

/// A task that shares the guardian's project directory blocks the review even
/// when none of its sessions set `review = "<<review:<id>>>"`. The guardian for
/// project-X must wait for BOTH tasks (A and B) to be Done, even though only A
/// declared the review.
#[test]
fn undeclared_overlapping_task_blocks_readiness() {
    // Both tasks get worktrees in the SAME git repository.
    let base = temp_base("overlap");
    let (cwd_a, cwd_b) = repo_with_two_worktrees(&base, "feature/a", "feature/b");

    // Task A: fast, declares a review. Task B: blocking, no review declaration.
    let toml = format!(
        "[[task]]\nname=\"a\"\n[[task.cell]]\ncwd=\"{cwd_a}\"\nprompt=\"p\"\nreview=\"<<review:r>>\"\n\
         [[task]]\nname=\"b\"\n[[task.cell]]\ncwd=\"{cwd_b}\"\nprompt=\"p\"\n\
         [[review]]\nid=\"r\"\nskip_auto_build=true\n"
    );
    let file: TaskFile = toml::from_str(&toml).unwrap();

    let store = Arc::new(Mutex::new(Store::open_in_memory().unwrap()));
    let (run_id, gid) = {
        let mut g = store.lock().unwrap();
        let run_id = g.insert_squad(&file, None, false).unwrap();
        let ids = derive_reviews(&g, &run_id, &file).expect("derive");
        assert_eq!(ids.len(), 1, "only one project so one review");
        (run_id, ids[0].clone())
    };

    let started = Arc::new(AtomicBool::new(false));
    let released = Arc::new(AtomicBool::new(false));
    let runner: Arc<dyn Runner> = Arc::new(GatableRunner {
        fast_prefix: cwd_a.clone(),
        started: Arc::clone(&started),
        released: Arc::clone(&released),
    });
    let token = CancelToken::new();

    let store2 = Arc::clone(&store);
    let runner2 = Arc::clone(&runner);
    let run_id2 = run_id.clone();
    let token2 = token.clone();
    // RAL-213: a fresh, private registry -- this test only exercises the
    // task-completion-triggered review-start path, not a guardian-merge
    // restart via that registry.
    let handle = std::thread::spawn(move || {
        execute_squad_with(
            &store2,
            runner2.as_ref(),
            &run_id2,
            &token2,
            &Cancellations::new(),
        )
    });

    // Wait until task B is blocking (task A has finished).
    poll_until(Duration::from_secs(5), "task B should have started", || {
        started.load(Ordering::SeqCst)
    });

    // Task A done but task B still running — guardian must NOT have started.
    let status = store.lock().unwrap().get_guardian(&gid).unwrap().status;
    assert_eq!(
        status, "collecting",
        "guardian should still be collecting while the overlapping task B is running"
    );

    // Release task B — both blocking tasks are now Done so the review should start.
    released.store(true, Ordering::SeqCst);

    poll_until(
        Duration::from_secs(5),
        "guardian should start after both blocking tasks are done",
        || store.lock().unwrap().get_guardian(&gid).unwrap().status != "collecting",
    );

    handle.join().unwrap();

    let _ = std::fs::remove_dir_all(&base);
}

// ── full end-to-end flow, gated on a local ollama stack ──────────────────────

/// Whether an ollama server is listening on the default port.
fn ollama_up() -> bool {
    use std::net::TcpStream;
    use std::time::Duration;
    "127.0.0.1:11434"
        .parse()
        .ok()
        .and_then(|addr| TcpStream::connect_timeout(&addr, Duration::from_millis(400)).ok())
        .is_some()
}

/// Whether ollama has `model` pulled (checked via its /api/tags).
fn ollama_has_model(model: &str) -> bool {
    match ureq::get("http://127.0.0.1:11434/api/tags").call() {
        Ok(resp) => resp
            .into_string()
            .map(|b| b.contains(model))
            .unwrap_or(false),
        Err(_) => false,
    }
}

/// Locate a `ralphus-runner` to drive: `RALPHUS_RUNNER_CMD`, else the dev venv.
fn find_runner() -> Option<String> {
    if let Ok(cmd) = std::env::var("RALPHUS_RUNNER_CMD") {
        if !cmd.trim().is_empty() {
            return Some(cmd);
        }
    }
    let ws = Path::new(env!("CARGO_MANIFEST_DIR")).parent()?;
    for rel in [
        "cli/.venv/Scripts/ralphus-runner.exe",
        "cli/.venv/bin/ralphus-runner",
    ] {
        let p = ws.join(rel);
        if p.exists() {
            return Some(p.to_string_lossy().into_owned());
        }
    }
    None
}

fn pydantic_ai_available(runner_cmd: &str) -> bool {
    let runner_path = Path::new(runner_cmd);
    let Some(dir) = runner_path.parent() else {
        return false;
    };
    for python in ["python.exe", "python3.exe", "python", "python3"] {
        let p = dir.join(python);
        if p.exists() {
            return Command::new(p)
                .args(["-c", "import pydantic_ai"])
                .output()
                .map(|o| o.status.success())
                .unwrap_or(false);
        }
    }
    false
}

/// One repo with two worktrees whose branches edit the SAME line differently, so
/// stacking the second onto the first conflicts. Returns their (cwd_a, cwd_b),
/// forward-slashed for TOML.
fn two_conflicting_worktrees(base: &Path) -> (String, String) {
    let repo = base.join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    init_repo(&repo);
    std::fs::write(repo.join("shared.txt"), "original line\n").unwrap();
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-m", "base"]);

    let wt_a = base.join("wt-a");
    let wt_b = base.join("wt-b");
    git(
        &repo,
        &["worktree", "add", "-b", "feature/a", wt_a.to_str().unwrap()],
    );
    git(
        &repo,
        &["worktree", "add", "-b", "feature/b", wt_b.to_str().unwrap()],
    );
    std::fs::write(wt_a.join("shared.txt"), "line changed by A\n").unwrap();
    git(&wt_a, &["commit", "-am", "a change"]);
    git(&wt_a, &["branch", "--set-upstream-to=main"]);
    std::fs::write(wt_b.join("shared.txt"), "line changed by B\n").unwrap();
    git(&wt_b, &["commit", "-am", "b change"]);
    git(&wt_b, &["branch", "--set-upstream-to=main"]);

    (
        wt_a.to_string_lossy().replace('\\', "/"),
        wt_b.to_string_lossy().replace('\\', "/"),
    )
}

/// The full gamut: a ticket-as-TOML is validated, submitted, run, and its review
/// (one project, two branches with a real merge conflict) is rebased — with the
/// conflict resolved by a live ollama agent. Skips unless ollama + a model +
/// `ralphus-runner` are all available locally.
#[test]
#[ignore = "calls a live local Ollama model; run explicitly with `cargo test -- --ignored`"]
fn full_flow_validate_submit_run_and_ollama_resolves_conflict() {
    let Some(runner_cmd) = find_runner() else {
        eprintln!("SKIP full_flow: ralphus-runner not found (set RALPHUS_RUNNER_CMD)");
        return;
    };
    if !pydantic_ai_available(&runner_cmd) {
        eprintln!(
            "SKIP full_flow: pydantic-ai not installed in runner environment (run `uv sync --extra runner` in cli/)"
        );
        return;
    }
    if !ollama_up() {
        eprintln!("SKIP full_flow: ollama not reachable on 127.0.0.1:11434");
        return;
    }
    let model = std::env::var("RALPHUS_RESOLVER_MODEL").unwrap_or_else(|_| "qwen3:8b".to_string());
    if !ollama_has_model(&model) {
        eprintln!("SKIP full_flow: ollama model '{model}' not pulled");
        return;
    }

    let base = temp_base("fullflow");
    let (cwd_a, cwd_b) = two_conflicting_worktrees(&base);

    // 1) The "ticket": a Task TOML. Validate it with the offline validator.
    let toml = format!(
        "[[task]]\nname=\"a\"\n\
         [[task.cell]]\ncwd=\"{cwd_a}\"\ncommand=\"noop\"\nreview=\"<<review:rev>>\"\n\
         [[task]]\nname=\"b\"\ndepends_on=[\"a\"]\n\
         [[task.cell]]\ncwd=\"{cwd_b}\"\ncommand=\"noop\"\nreview=\"<<review:rev>>\"\n\
         [[review]]\nid=\"rev\"\nskip_auto_build=true\n"
    );
    assert!(
        ralphus_core::validate::validate_toml(&toml).is_ok(),
        "ticket TOML must validate: {:?}",
        ralphus_core::validate::validate_toml(&toml).errors
    );
    let file: TaskFile = toml::from_str(&toml).unwrap();

    // 2) Submit (ingest) and 3) run the tasks to success.
    let store = Arc::new(Mutex::new(Store::open_in_memory().unwrap()));
    let run_id = {
        let mut g = store.lock().unwrap();
        g.insert_squad(&file, Some("ticket-42"), false).unwrap()
    };
    execute_squad(&store, &OkRunner, &run_id);
    assert_eq!(
        store.lock().unwrap().squad_state(&run_id).unwrap(),
        SquadState::Done
    );

    // 4) Derive the review: both worktrees are one project -> one guardian with
    //    two branches in topological order (a then b). Derived explicitly here
    //    (rather than via submit's auto-start) so we can inject a real ollama
    //    runner for the conflict resolution.
    let ids = {
        let g = store.lock().unwrap();
        derive_reviews(&g, &run_id, &file).expect("derive")
    };
    assert_eq!(ids.len(), 1, "one project -> one review");
    let gid = ids[0].clone();
    assert_eq!(
        store
            .lock()
            .unwrap()
            .get_guardian(&gid)
            .unwrap()
            .branches
            .len(),
        2
    );

    // 5) Build the review. feature/b conflicts with feature/a on shared.txt; the
    //    live agent must resolve it for the stack to reach review.
    let runner = SubprocessRunner::new(&runner_cmd);
    run_merge(&store, &runner, &gid);

    let view = store.lock().unwrap().get_guardian(&gid).unwrap();
    assert_eq!(view.status, "in_review", "detail: {:?}", view.detail);
    assert!(
        view.branches
            .iter()
            .any(|b| b.merge_status == "conflict_resolved"),
        "a branch should be conflict-resolved by the agent: {:?}",
        view.branches
            .iter()
            .map(|b| (b.branch.clone(), b.merge_status.clone()))
            .collect::<Vec<_>>()
    );
    let combined = view.combined_worktree.expect("combined worktree");
    let merged = std::fs::read_to_string(Path::new(&combined).join("shared.txt")).unwrap();
    assert!(
        !merged.contains("<<<<<<<"),
        "conflict markers remain: {merged}"
    );

    let _ = std::fs::remove_dir_all(&base);
}

#[test]
fn no_review_declaration_makes_no_guardians() {
    let mut store = Store::open_in_memory().unwrap();
    let toml = "[[task]]\nname=\"t\"\n[[task.cell]]\ncwd=\"/tmp\"\nprompt=\"p\"\n";
    let file: TaskFile = toml::from_str(toml).unwrap();
    let run_id = store.insert_squad(&file, None, false).unwrap();
    // No git access happens because no session declares a review.
    assert!(derive_reviews(&store, &run_id, &file).unwrap().is_empty());
    assert!(store.guardians_for_squad(&run_id).unwrap().is_empty());
}

// ── Multi-project tests (RAL-29) ─────────────────────────────────────────────

/// Two sessions in different repos, linked by the same `ralphus:new-review/<key>`
/// id, should produce ONE shared guardian whose branches are tagged with their
/// respective project roots.
#[test]
fn link_key_across_two_repos_creates_one_multi_project_guardian() {
    let base_a = temp_base("mp-link-a");
    let base_b = temp_base("mp-link-b");
    let cwd_a = repo_with_worktree(&base_a, "feature/a");
    let cwd_b = repo_with_worktree(&base_b, "feature/b");

    let mut store = Store::open_in_memory().unwrap();

    // One submission with two sessions (repo A + repo B) sharing the key mints one
    // multi-project guardian.
    let file: TaskFile = toml::from_str(&two_session_toml(
        &cwd_a,
        &cwd_b,
        "ralphus:new-review/cross",
        "name=\"Cross Review\"\nskip_auto_build=true",
    ))
    .unwrap();
    let run = store.insert_squad(&file, None, false).unwrap();
    let ids = derive_reviews(&store, &run, &file).expect("derive");
    assert_eq!(ids.len(), 1, "one submission mints one guardian");
    let gid = ids[0].clone();

    // The guardian has both branches.
    let g = store.get_guardian(&gid).unwrap();
    let branches: Vec<_> = g.branches.iter().map(|b| b.branch.as_str()).collect();
    assert_eq!(branches, vec!["feature/a", "feature/b"]);

    // Each branch is tagged with its own project root (different directories).
    let proj_a = g.branches[0]
        .project
        .clone()
        .expect("branch a has a project");
    let proj_b = g.branches[1]
        .project
        .clone()
        .expect("branch b has a project");
    assert_ne!(
        proj_a, proj_b,
        "branches from different repos must have different project roots"
    );

    // `g.projects` reports both distinct roots in branch order.
    assert_eq!(g.projects.len(), 2, "two projects");
    assert_eq!(g.projects[0], proj_a);
    assert_eq!(g.projects[1], proj_b);

    let _ = std::fs::remove_dir_all(&base_a);
    let _ = std::fs::remove_dir_all(&base_b);
}

/// A single-project link group (both sessions in the same repo) should produce
/// ONE guardian whose branches have no project tag (NULL / None), so the merge
/// engine uses guardian.git_root for both — identical to the pre-RAL-29 behavior.
#[test]
fn link_key_same_repo_branches_have_no_project_tag() {
    let base = temp_base("mp-link-same");
    let (cwd_a, cwd_b) = repo_with_two_worktrees(&base, "feature/a", "feature/b");

    let mut store = Store::open_in_memory().unwrap();

    // One submission, two sessions in the SAME repo, sharing the key.
    let file: TaskFile = toml::from_str(&two_session_toml(
        &cwd_a,
        &cwd_b,
        "ralphus:new-review/same",
        "name=\"Same Repo\"\nskip_auto_build=true",
    ))
    .unwrap();
    let run = store.insert_squad(&file, None, false).unwrap();
    let ids = derive_reviews(&store, &run, &file).expect("derive");

    let g = store.get_guardian(&ids[0]).unwrap();
    // Branches in the same repo still carry a project tag (it just happens to be
    // the same path for both), so the merge engine partitions them into one group.
    assert_eq!(g.branches.len(), 2);
    // Both branches should resolve to the same project root.
    let p0 = g.branches[0].project.clone();
    let p1 = g.branches[1].project.clone();
    assert_eq!(p0, p1, "same-repo branches have the same project tag");
    // `g.projects` should contain exactly one entry.
    assert_eq!(g.projects.len(), 1);

    let _ = std::fs::remove_dir_all(&base);
}

/// Proj-group reviews (no link key) are still one-guardian-per-project and the
/// branches in each guardian carry NO project tag (they use guardian.git_root).
#[test]
fn proj_group_branches_carry_no_project_tag() {
    let base_a = temp_base("mp-proj-a");
    let base_b = temp_base("mp-proj-b");
    let cwd_a = repo_with_worktree(&base_a, "feature/a");
    let cwd_b = repo_with_worktree(&base_b, "feature/b");

    let toml = format!(
        "[[task]]\nname=\"t\"\n\
         [[task.cell]]\ncwd=\"{cwd_a}\"\nprompt=\"p\"\nreview=\"<<review:rA>>\"\n\
         [[task.cell]]\ncwd=\"{cwd_b}\"\nprompt=\"p\"\nreview=\"<<review:rB>>\"\n\
         [[review]]\nid=\"rA\"\nskip_auto_build=true\n\
         [[review]]\nid=\"rB\"\nskip_auto_build=true\n"
    );
    let file: TaskFile = toml::from_str(&toml).unwrap();
    let mut store = Store::open_in_memory().unwrap();
    let run_id = store.insert_squad(&file, None, false).unwrap();
    let ids = derive_reviews(&store, &run_id, &file).expect("derive ok");

    assert_eq!(ids.len(), 2, "two projects -> two guardians");
    for id in &ids {
        let g = store.get_guardian(id).unwrap();
        // Single-project guardians: each has exactly one project (its git_root).
        assert_eq!(g.projects.len(), 1);
        assert_eq!(g.projects[0], g.git_root);
        // Branches in single-project guardians carry no explicit project tag.
        for b in &g.branches {
            assert!(
                b.project.is_none(),
                "proj-group branches should have no project tag"
            );
        }
    }

    let _ = std::fs::remove_dir_all(&base_a);
    let _ = std::fs::remove_dir_all(&base_b);
}

/// Multi-project guardian: the merge engine processes each project independently
/// and all must succeed for the guardian to reach `InReview`.
#[test]
fn multi_project_merge_runs_per_project_and_aggregates() {
    let base_a = temp_base("mp-merge-a");
    let base_b = temp_base("mp-merge-b");
    let cwd_a = repo_with_worktree(&base_a, "feature/a");
    let cwd_b = repo_with_worktree(&base_b, "feature/b");

    // Commit something on each feature branch so the stack is non-empty.
    let cwd_a_native = cwd_a.replace('/', std::path::MAIN_SEPARATOR_STR);
    let cwd_b_native = cwd_b.replace('/', std::path::MAIN_SEPARATOR_STR);
    let wt_a = Path::new(&cwd_a_native);
    let wt_b = Path::new(&cwd_b_native);
    std::fs::write(wt_a.join("a.txt"), "change by a\n").unwrap();
    git(wt_a, &["add", "."]);
    git(wt_a, &["commit", "-m", "a change"]);
    std::fs::write(wt_b.join("b.txt"), "change by b\n").unwrap();
    git(wt_b, &["add", "."]);
    git(wt_b, &["commit", "-m", "b change"]);

    let store = Arc::new(Mutex::new(Store::open_in_memory().unwrap()));

    // One submission with two sessions (repo A + repo B) sharing the key.
    let file: TaskFile = toml::from_str(&two_session_toml(
        &cwd_a,
        &cwd_b,
        "ralphus:new-review/mp",
        "name=\"MP Review\"\nskip_auto_build=true",
    ))
    .unwrap();
    let gid = {
        let mut g = store.lock().unwrap();
        let r = g.insert_squad(&file, None, false).unwrap();
        let ids = derive_reviews(&g, &r, &file).expect("derive");
        ids[0].clone()
    };

    // The guardian now has branches from two different repos.
    let g = store.lock().unwrap().get_guardian(&gid).unwrap();
    assert_eq!(g.projects.len(), 2, "two distinct projects");

    // Run the merge. Each project's branches are stacked independently.
    // OkRunner never touches disk; the rebase works on real worktrees.
    ralphus_daemon::guardian_merge::run_merge(&store, &OkRunner, &gid);

    let view = store.lock().unwrap().get_guardian(&gid).unwrap();
    assert_eq!(
        view.status, "in_review",
        "all projects merged -> guardian reaches in_review; detail: {:?}",
        view.detail
    );
    // Each project's branch must be done (or conflict_resolved).
    for b in &view.branches {
        assert!(
            matches!(b.merge_status.as_str(), "done" | "conflict_resolved"),
            "branch {} status: {}",
            b.branch,
            b.merge_status
        );
    }

    let _ = std::fs::remove_dir_all(&base_a);
    let _ = std::fs::remove_dir_all(&base_b);
}

/// If one project's branch fails to merge (e.g. its base branch doesn't exist),
/// the guardian becomes `merge_failed` even if the other project's branches are
/// fine (all-must-pass aggregation).
#[test]
fn multi_project_all_must_pass_one_fails_makes_merge_failed() {
    let base_a = temp_base("mp-fail-a");
    // We only create a real repo for A; B is left as a non-existent path.
    let cwd_a = repo_with_worktree(&base_a, "feature/a");
    let cwd_b = format!("{}/nonexistent/path", base_a.display());

    // Manually build a guardian with branches from two "projects" — one real, one fake.
    let store = Arc::new(Mutex::new(Store::open_in_memory().unwrap()));
    let gid = {
        let g = store.lock().unwrap();
        let id = g.create_guardian("fail test", "main", &cwd_a).unwrap();
        g.add_guardian_branch_with_project(&id, "feature/a", None)
            .unwrap();
        // Branch "feature/b" belongs to a non-existent project root.
        g.add_guardian_branch_with_project(&id, "feature/b", Some(&cwd_b))
            .unwrap();
        id
    };

    ralphus_daemon::guardian_merge::run_merge(&store, &OkRunner, &gid);

    let view = store.lock().unwrap().get_guardian(&gid).unwrap();
    assert_eq!(
        view.status, "merge_failed",
        "invalid project root must cause merge_failed"
    );

    let _ = std::fs::remove_dir_all(&base_a);
}

/// `GuardianView.projects` for a single-project guardian (no branch project tags)
/// always contains exactly one entry equal to `git_root`.
#[test]
fn single_project_guardian_projects_field_has_one_entry() {
    let store = Store::open_in_memory().unwrap();
    let id = store.create_guardian("r", "main", "/some/repo").unwrap();
    store.add_guardian_branch(&id, "feature/a").unwrap();
    store.add_guardian_branch(&id, "feature/b").unwrap();

    let g = store.get_guardian(&id).unwrap();
    assert_eq!(g.projects, vec!["/some/repo".to_string()]);
    assert_eq!(g.projects[0], g.git_root);
}

/// `add_guardian_branch_with_project` correctly stores and retrieves the project
/// tag, while `add_guardian_branch` leaves it as None.
#[test]
fn branch_project_tag_stored_and_retrieved() {
    let store = Store::open_in_memory().unwrap();
    let id = store.create_guardian("r", "main", "/primary").unwrap();
    store.add_guardian_branch(&id, "feature/a").unwrap();
    store
        .add_guardian_branch_with_project(&id, "feature/b", Some("/secondary"))
        .unwrap();

    let g = store.get_guardian(&id).unwrap();
    assert_eq!(g.branches[0].project, None, "no explicit project -> None");
    assert_eq!(
        g.branches[1].project.as_deref(),
        Some("/secondary"),
        "explicit project stored"
    );
    assert_eq!(g.projects, vec!["/primary", "/secondary"]);
}

/// `set_guardian_project_base_commit` updates the JSON map and the legacy column.
#[test]
fn per_project_base_commits_stored_and_retrieved() {
    let store = Store::open_in_memory().unwrap();
    let id = store.create_guardian("r", "main", "/primary").unwrap();

    // Record a commit for the primary project (also updates legacy base_commit).
    store
        .set_guardian_project_base_commit(&id, "/primary", "abc123")
        .unwrap();
    let g = store.get_guardian(&id).unwrap();
    assert_eq!(
        g.base_commits.get("/primary").map(String::as_str),
        Some("abc123")
    );
    assert_eq!(
        g.base_commit.as_deref(),
        Some("abc123"),
        "legacy column updated"
    );

    // Record a commit for a secondary project (only updates the JSON map).
    store
        .set_guardian_project_base_commit(&id, "/secondary", "def456")
        .unwrap();
    let g = store.get_guardian(&id).unwrap();
    assert_eq!(
        g.base_commits.get("/secondary").map(String::as_str),
        Some("def456")
    );
    assert_eq!(
        g.base_commit.as_deref(),
        Some("abc123"),
        "legacy column unchanged for non-primary"
    );
    assert_eq!(g.base_commits.len(), 2);
}

// ── RAL-159: implicit worktree-sharing review membership ────────────────────

/// A session with no `review = "<<review:<id>>>"` of its own, but whose cwd is a nested
/// subfolder of another session's review-linked worktree, implicitly joins
/// that same branch's review (matched by literal worktree root, not mere
/// project identity) -- so it shows up in the session's "in reviews" list
/// (RAL-17) exactly like the explicitly-linked session. A THIRD session in a
/// DIFFERENT linked worktree of the SAME repo must NOT match, proving the
/// match is worktree-scoped rather than project-scoped (project identity
/// already collapses every linked worktree of a repo together, per
/// `undeclared_overlapping_task_blocks_readiness` above -- this is a
/// narrower, worktree-literal match, not a re-run of that broader check).
#[test]
fn nested_cwd_session_implicitly_joins_review_and_reviews_list() {
    let base = temp_base("worktree-share");
    let (cwd_share, cwd_other) = repo_with_two_worktrees(&base, "feature/share", "feature/other");
    let sub = format!("{cwd_share}/sub");
    std::fs::create_dir_all(sub.replace('/', std::path::MAIN_SEPARATOR_STR)).unwrap();

    let toml = format!(
        "[[task]]\nname=\"a\"\n[[task.cell]]\ncwd=\"{cwd_share}\"\nprompt=\"p\"\nreview=\"<<review:r>>\"\n\
         [[task]]\nname=\"b\"\n[[task.cell]]\ncwd=\"{sub}\"\nprompt=\"p\"\n\
         [[task]]\nname=\"c\"\n[[task.cell]]\ncwd=\"{cwd_other}\"\nprompt=\"p\"\n\
         [[review]]\nid=\"r\"\nskip_auto_build=true\n"
    );
    let file: TaskFile = toml::from_str(&toml).unwrap();

    let mut store = Store::open_in_memory().unwrap();
    let run_id = store.insert_squad(&file, None, false).unwrap();
    let ids = derive_reviews(&store, &run_id, &file).expect("derive ok");

    assert_eq!(ids.len(), 1, "only one project declares a review");
    let gid = ids[0].clone();
    let g = store.get_guardian(&gid).unwrap();
    assert_eq!(g.branches.len(), 1);
    assert_eq!(g.branches[0].branch, "feature/share");

    let view = store.get_squad(&run_id).unwrap();
    let a_reviews = view.tasks[0].cells[0].reviews.clone();
    let b_reviews = view.tasks[1].cells[0].reviews.clone();
    let c_reviews = view.tasks[2].cells[0].reviews.clone();

    assert_eq!(a_reviews.len(), 1, "explicit session is in the review");
    assert_eq!(
        b_reviews.len(),
        1,
        "nested-cwd sibling implicitly joins the same review"
    );
    assert_eq!(a_reviews[0].id, b_reviews[0].id);
    assert_eq!(a_reviews[0].branch, b_reviews[0].branch);
    assert!(
        c_reviews.is_empty(),
        "a session in a DIFFERENT worktree of the same repo must not match"
    );

    let _ = std::fs::remove_dir_all(&base);
}

/// The review-ready gate withholds `MergeStatus::Ready` on a branch until
/// EVERY session sharing that worktree is done -- not just the explicitly
/// review-linked one. Once the last sibling (here, the implicit nested-cwd
/// session) finishes, the branch promotes to `ready` in that same call.
#[test]
fn worktree_sharing_gates_branch_ready_until_all_sessions_done() {
    let base = temp_base("worktree-gate");
    let cwd = repo_with_worktree(&base, "feature/gate");
    let sub = format!("{cwd}/sub");
    std::fs::create_dir_all(sub.replace('/', std::path::MAIN_SEPARATOR_STR)).unwrap();

    let toml = format!(
        "[[task]]\nname=\"a\"\n[[task.cell]]\ncwd=\"{cwd}\"\nprompt=\"p\"\nreview=\"<<review:r>>\"\n\
         [[task]]\nname=\"b\"\n[[task.cell]]\ncwd=\"{sub}\"\nprompt=\"p\"\n\
         [[review]]\nid=\"r\"\nskip_auto_build=true\n"
    );
    let file: TaskFile = toml::from_str(&toml).unwrap();

    let mut store = Store::open_in_memory().unwrap();
    let run_id = store.insert_squad(&file, None, false).unwrap();
    let ids = derive_reviews(&store, &run_id, &file).expect("derive ok");
    let gid = ids[0].clone();

    // Task A (explicit) finishes -- branch must stay `pending`: task B (the
    // implicit worktree sibling) hasn't finished yet.
    store
        .set_cell_state(&run_id, 0, 0, NodeState::Done)
        .unwrap();
    let n = store.mark_ready_branches_with_done_cells(&gid).unwrap();
    assert_eq!(n, 0, "must not promote while a sibling is still pending");
    assert_eq!(
        store.get_guardian(&gid).unwrap().branches[0].merge_status,
        "pending"
    );

    // Task B finishes too -- now every worktree-sharing session is done.
    store
        .set_cell_state(&run_id, 1, 0, NodeState::Done)
        .unwrap();
    let n = store.mark_ready_branches_with_done_cells(&gid).unwrap();
    assert_eq!(n, 1, "promotes exactly the one branch");
    assert_eq!(
        store.get_guardian(&gid).unwrap().branches[0].merge_status,
        "ready"
    );

    let _ = std::fs::remove_dir_all(&base);
}

/// Two sessions sharing a worktree finishing at (near-)simultaneously must
/// transition the branch to `ready` exactly once -- no double-trigger, no
/// missed trigger (RAL-159 interview Q4). Phase 1 races both sessions' `done`
/// writes to land as close together as possible; phase 2 then races two
/// threads both calling the readiness check right after, simulating two
/// scheduler task-completion paths discovering "all done" at once. The daemon
/// serializes all `Store` access through one `Mutex`, and the promotion SQL
/// itself is guarded by `WHERE merge_status='pending'`, so only one of the
/// racing calls can ever actually flip it -- no extra locking is needed.
#[test]
fn simultaneous_worktree_sibling_completion_transitions_ready_exactly_once() {
    let base = temp_base("worktree-race");
    let cwd = repo_with_worktree(&base, "feature/race");
    let sub = format!("{cwd}/sub");
    std::fs::create_dir_all(sub.replace('/', std::path::MAIN_SEPARATOR_STR)).unwrap();

    let toml = format!(
        "[[task]]\nname=\"a\"\n[[task.cell]]\ncwd=\"{cwd}\"\nprompt=\"p\"\nreview=\"<<review:r>>\"\n\
         [[task]]\nname=\"b\"\n[[task.cell]]\ncwd=\"{sub}\"\nprompt=\"p\"\n\
         [[review]]\nid=\"r\"\nskip_auto_build=true\n"
    );
    let file: TaskFile = toml::from_str(&toml).unwrap();

    let store = Arc::new(Mutex::new(Store::open_in_memory().unwrap()));
    let (run_id, gid) = {
        let mut g = store.lock().unwrap();
        let run_id = g.insert_squad(&file, None, false).unwrap();
        let ids = derive_reviews(&g, &run_id, &file).expect("derive");
        (run_id, ids[0].clone())
    };

    // Phase 1: race both sessions' `done` transitions.
    let barrier = Arc::new(std::sync::Barrier::new(2));
    let set_handles: Vec<_> = [(0i64, 0i64), (1i64, 0i64)]
        .into_iter()
        .map(|(task_idx, idx)| {
            let store = Arc::clone(&store);
            let run_id = run_id.clone();
            let barrier = Arc::clone(&barrier);
            std::thread::spawn(move || {
                barrier.wait();
                store
                    .lock()
                    .unwrap()
                    .set_cell_state(&run_id, task_idx, idx, NodeState::Done)
                    .unwrap();
            })
        })
        .collect();
    for h in set_handles {
        h.join().unwrap();
    }

    // Phase 2: race two concurrent "task just finished" readiness checks.
    let barrier2 = Arc::new(std::sync::Barrier::new(2));
    let mark_handles: Vec<_> = (0..2)
        .map(|_| {
            let store = Arc::clone(&store);
            let gid = gid.clone();
            let barrier2 = Arc::clone(&barrier2);
            std::thread::spawn(move || {
                barrier2.wait();
                store
                    .lock()
                    .unwrap()
                    .mark_ready_branches_with_done_cells(&gid)
                    .unwrap()
            })
        })
        .collect();
    let total: usize = mark_handles.into_iter().map(|h| h.join().unwrap()).sum();

    assert_eq!(
        total, 1,
        "the branch must be promoted to ready exactly once, not zero or twice"
    );
    assert_eq!(
        store.lock().unwrap().get_guardian(&gid).unwrap().branches[0].merge_status,
        "ready"
    );

    let _ = std::fs::remove_dir_all(&base);
}
