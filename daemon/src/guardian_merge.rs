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

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex};

use crate::guardian::{GuardianStatus, MergeStatus};
use crate::runner::{Runner, RunnerSpec};
use crate::scheduler::Semaphore;
use crate::server::Reply;
use crate::store::Store;

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
fn head_hash(wt: &Path) -> Option<String> {
    git(wt, &["rev-parse", "HEAD"])
        .ok()
        .map(|s| s.trim().to_string())
}

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
fn relink_worktree(root: &Path, wt: &Path) -> std::io::Result<()> {
    let name = wt
        .file_name()
        .expect("worktree path has no filename component")
        .to_string_lossy();
    let entry_dir = root.join(".git").join("worktrees").join(name.as_ref());
    std::fs::create_dir_all(&entry_dir)?;
    let wt_git_str = wt.join(".git").to_string_lossy().replace('\\', "/");
    std::fs::write(entry_dir.join("gitdir"), format!("{wt_git_str}\n"))?;
    std::fs::write(entry_dir.join("commondir"), b"../..\n")?;
    let entry_str = entry_dir.to_string_lossy().replace('\\', "/");
    std::fs::write(wt.join(".git"), format!("gitdir: {entry_str}\n"))?;
    Ok(())
}

/// Set up `wt` as a worktree for `branch` under the review-branch name `rev`.
/// Equivalent to `git worktree add -B <rev> <wt> <branch>`, but when the
/// directory already exists and cannot be deleted (e.g., it is a process's CWD
/// on Windows), re-registers the existing directory and resets it in-place
/// instead of failing — preserving the directory for any process using it.
fn worktree_add_or_reset(
    root: &Path,
    rev: &str,
    wt: &Path,
    branch: &str,
) -> std::result::Result<(), String> {
    let wt_str = wt.to_string_lossy().to_string();
    let _ = git(root, &["worktree", "remove", "--force", &wt_str]);
    if !wt.exists() {
        return git(root, &["worktree", "add", "-B", rev, &wt_str, branch]).map(|_| ());
    }
    // Directory survived (CWD lock or similar). Re-register it so git can
    // work inside it, abort any in-progress rebase, then reset to the
    // requested starting point.
    relink_worktree(root, wt)
        .map_err(|e| format!("cannot relink worktree at {wt_str}: {e}"))?;
    let _ = git(wt, &["rebase", "--abort"]);
    git(wt, &["checkout", "-f", "-B", rev, branch]).map(|_| ())
}

/// Whether a rebase is currently in progress in `wt` (its state directory
/// exists). Used to decide whether `rebase --continue` still has work to do.
fn rebase_in_progress(wt: &Path) -> bool {
    ["rebase-merge", "rebase-apply"].iter().any(|d| {
        git(wt, &["rev-parse", "--git-path", d])
            .ok()
            .map(|p| {
                let p = p.trim();
                let path = Path::new(p);
                let full = if path.is_absolute() {
                    path.to_path_buf()
                } else {
                    wt.join(path)
                };
                !p.is_empty() && full.exists()
            })
            .unwrap_or(false)
    })
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

/// The agent backend used to resolve conflicts: the review's own `stored` agent
/// (from `[[task.session.review]]`), else the `RALPHUS_RESOLVER_AGENT` env
/// override, else `ollama`.
fn resolver_agent(stored: Option<&str>) -> String {
    stored
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(ToString::to_string)
        .or_else(|| std::env::var("RALPHUS_RESOLVER_AGENT").ok())
        .unwrap_or_else(|| "ollama".to_string())
}

/// The model the resolver runs: the review's own `stored` model, else the
/// `RALPHUS_RESOLVER_MODEL` env override, else `qwen3:8b` for the ollama backend
/// (other backends take their own default when unset).
fn resolver_model(stored: Option<&str>, agent: &str) -> Option<String> {
    stored
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(ToString::to_string)
        .or_else(|| std::env::var("RALPHUS_RESOLVER_MODEL").ok())
        .or_else(|| (agent == "ollama").then(|| "qwen3:8b".to_string()))
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

    let (skip_checks, explicit_checks, git_root) = {
        let guard = store.lock().expect("poisoned");
        let skip = guard.guardian_skip_checks(id).unwrap_or(false);
        let checks = guard.guardian_checks(id).unwrap_or_default();
        let root = guard
            .get_guardian(id)
            .map(|g| g.git_root)
            .unwrap_or_default();
        (skip, checks, root)
    };

    // Opt-out: user has disabled verification for this review entirely.
    if skip_checks {
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
        run_id: "guardian".to_string(),
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
        verify: false,
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
/// is hit. Returns Ok when fully resolved.
fn resolve_conflicts_with_agent(
    store: &Arc<Mutex<Store>>,
    id: &str,
    position: i64,
    runner: &dyn Runner,
    wt: &Path,
    branch: &str,
) -> std::result::Result<(), String> {
    // The review's configured resolver backend/model (falls back to env/default).
    let (agent, model) = {
        let guard = store.lock().expect("poisoned");
        let g = guard.get_guardian(id).ok();
        let stored_agent = g.as_ref().and_then(|g| g.resolver_agent.clone());
        let stored_model = g.and_then(|g| g.resolver_model.clone());
        let agent = resolver_agent(stored_agent.as_deref());
        let model = resolver_model(stored_model.as_deref(), &agent);
        (agent, model)
    };

    // Derive quality-bar instructions from the task's verify steps (synthesised
    // once per branch rebase attempt; result is reused across loop iterations).
    let quality_note = synthesize_verify_instructions(store, id, branch, runner, &agent, &model);

    // Seed the live progress (total markers for this branch) so the review page
    // can show "resolved / total" while the agent works.
    let total = i64::try_from(count_markers(wt, &conflicted_files(wt))).unwrap_or(i64::MAX);
    let set_progress = |remaining: i64| {
        let guard = store.lock().expect("poisoned");
        let _ = guard.set_guardian_conflicts(id, Some(total), Some(remaining));
        let _ = guard.set_branch_conflicts(id, position, Some(total), Some(remaining));
    };
    set_progress(total);
    for _ in 0..32 {
        let files = conflicted_files(wt);
        if files.is_empty() {
            // No conflicts left: the rebase has either finished or auto-advanced
            // through clean commits. If it is still in progress, drive it forward;
            // once it reports no rebase in progress we are done.
            if rebase_in_progress(wt) {
                if git(wt, &["rebase", "--continue"]).is_err() {
                    let _ = git(wt, &["rebase", "--skip"]);
                }
                continue;
            }
            set_progress(0);
            return Ok(());
        }
        let prompt = format!(
            "Resolve all merge conflict markers in these files from branch '{branch}': {}. \
             Read each file, intelligently merge both sides of every conflict block \
             (<<<<<<<...=======...>>>>>>>), and write the resolved content back with \
             ALL markers removed.{quality_note}",
            files.join(", ")
        );
        let system_prompt = "You are a git merge-conflict resolver running inside a checked-out worktree \
             during an active `git rebase`. Your job is to eliminate every conflict marker, \
             produce correctly merged files, and satisfy any quality requirements listed in the prompt.\n\
             \n\
             Step-by-step:\n\
             1. For each conflicted file named in the prompt: call read_file to get its \
                current content.\n\
             2. Locate every conflict block delimited by <<<<<<< ... ======= ... >>>>>>>. \
                Understand what each side contributes and write the correct merged result — \
                preserving the intent of both sides, with ALL markers removed.\n\
             3. Call write_file with the fully resolved content. Repeat for every file.\n\
             4. If the prompt lists quality requirements (formatters, linters, tests), run \
                them with run_bash and fix any failures. For project-specific instructions, \
                look for CLAUDE.md or AGENTS.md in the repository root.\n\
             5. Once all files are marker-free and quality requirements are met, call \
                run_bash with exactly: git add -A\n\
             \n\
             Do NOT call `git rebase --continue`, `git commit`, `git push`, or any other \
             git command besides `git add -A`. The orchestrator advances the rebase after \
             you finish.";
        let spec = RunnerSpec {
            run_id: "guardian".to_string(),
            task: "resolve".to_string(),
            session_id: "resolver".to_string(),
            cwd: wt.to_string_lossy().into_owned(),
            prompt: Some(prompt),
            command: None,
            agent: agent.clone(),
            model: model.clone(),
            system_prompt: Some(system_prompt.to_string()),
            system_prompt_position: None,
            timeout_sec: None,
            budget_tokens: None,
            verify: false,
        };
        let result = runner.run(&spec);
        if !result.is_done() {
            let err = result
                .error
                .as_deref()
                .unwrap_or("resolver subprocess produced no output");
            return Err(format!("conflict resolver failed: {err}"));
        }

        let remaining = count_markers(wt, &files);
        set_progress(i64::try_from(remaining).unwrap_or(i64::MAX));
        if remaining > 0 {
            // Markers still present — let the loop retry up to the cap rather than
            // failing immediately. The agent may need more than one pass to fully
            // clear all conflicts (e.g. partial resolution or a multi-file case).
            continue;
        }
        git(wt, &["add", "-A"])?;
        // Advance the rebase. `--continue` may fail if the resolved commit is now
        // empty (its change already applied) — drop it with `--skip`. Either way
        // the loop re-checks and resolves any further conflicting commits.
        if git(wt, &["rebase", "--continue"]).is_err() {
            let _ = git(wt, &["rebase", "--skip"]);
        }
    }
    Err("exceeded conflict-resolution attempts".to_string())
}

/// CCTL-134: run the review's check gates against a single stacked commit's
/// worktree. Returns `Err` with the failing command on the first failure. A
/// review that opted out of checks (CCTL-130) or declares none passes trivially.
fn run_commit_checks(
    store: &Arc<Mutex<Store>>,
    id: &str,
    wt: &Path,
    branch: &str,
) -> std::result::Result<(), String> {
    let (skip, checks) = {
        let guard = store.lock().expect("poisoned");
        (
            guard.guardian_skip_checks(id).unwrap_or(false),
            guard.guardian_checks(id).unwrap_or_default(),
        )
    };
    if skip {
        return Ok(());
    }
    let wt_str = wt.to_string_lossy();
    for cmd in &checks {
        if !crate::verify::run_command_verify(&wt_str, cmd) {
            return Err(format!("check failed after '{branch}': {cmd}"));
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
    root: &Path,
    wt_base: &Path,
    base_branch: &str,
    from_position: i64,
    set_status: &F,
) {
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
    let mut prev_ref = branches
        .iter()
        .find(|b| b.position == from_position)
        .map(|b| format!("guardian/{id}/wt-{}", b.branch))
        .unwrap_or_default();
    for ob in branches.iter().filter(|b| b.position > from_position) {
        let _ = store.lock().expect("poisoned").set_branch_status(
            id,
            ob.position,
            MergeStatus::InProgress,
            None,
        );
        let rev = format!("guardian/{id}/wt-{}", ob.branch);
        let wt_j = wt_base.join(format!("wt-{}", ob.branch));
        let wt_j_str = wt_j.to_string_lossy().to_string();
        if let Err(e) = worktree_add_or_reset(root, &rev, &wt_j, &ob.branch) {
            fail_branch(store, id, ob.position, &ob.branch, &e, set_status);
            return;
        }
        if stack_pick(
            store,
            runner,
            id,
            ob.position,
            &ob.branch,
            &base_sha,
            &prev_ref,
            &rev,
            &wt_j,
        )
        .is_err()
        {
            return;
        }
        if let Err(e) = run_commit_checks(store, id, &wt_j, &ob.branch) {
            fail_branch(store, id, ob.position, &ob.branch, &e, set_status);
            return;
        }
        let _ = store
            .lock()
            .expect("poisoned")
            .set_branch_review(id, ob.position, &rev, &wt_j_str);
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
            // RAL-27: regenerate manual review commands after re-stacking.
            generate_manual_commands(
                store,
                runner,
                id,
                root,
                &base_sha,
                &prev_ref,
                Some(&wt_base.join(format!("{id}-review"))),
            );
            set_status(GuardianStatus::InReview, note.as_deref());
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
    let git_root = PathBuf::from(&guardian.git_root);
    let wt_base = worktree_dir(&guardian.git_root, id);
    let r_agent = resolver_agent(guardian.resolver_agent.as_deref());
    let r_model = resolver_model(guardian.resolver_model.as_deref(), &r_agent);

    struct Target {
        position: i64,
        branch: String,
        wt: PathBuf,
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
            let wt = PathBuf::from(wt_str);
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
            let branch = t.branch.clone();
            let wt_cwd = t.wt.to_string_lossy().into_owned();
            let wt_for_thread = t.wt.clone();
            let instructions = t.instructions.clone();
            let r_agent = r_agent.clone();
            let r_model = r_model.clone();
            let runner_clone = runner.clone();
            std::thread::spawn(move || {
                // Stash any pre-existing dirty state so we only commit agent-made
                // changes (not leftovers from a prior no-commit turn).
                let stashed = if !no_commit {
                    let pre = git(&wt_for_thread, &["status", "--porcelain"]).unwrap_or_default();
                    if !pre.trim().is_empty() {
                        git(&wt_for_thread, &["stash", "--include-untracked"]).is_ok()
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
                    run_id: "guardian".to_string(),
                    task: "route".to_string(),
                    session_id: format!("route-{}", branch.replace(['/', '.'], "-")),
                    cwd: wt_cwd,
                    prompt: Some(prompt),
                    command: None,
                    agent: r_agent,
                    model: r_model,
                    system_prompt: None,
                    system_prompt_position: None,
                    timeout_sec: None,
                    budget_tokens: None,
                    verify: false,
                };
                let _ = runner_clone.run(&spec);
                // Defensive amend: if the agent left uncommitted changes and we are
                // allowed to commit, finalize them now.
                if !no_commit {
                    let status =
                        git(&wt_for_thread, &["status", "--porcelain"]).unwrap_or_default();
                    if !status.trim().is_empty() {
                        let _ = git(&wt_for_thread, &["add", "-A"]);
                        let _ = git(&wt_for_thread, &["commit", "--amend", "--no-edit"]);
                    }
                }
                // Restore any pre-existing (no-commit) changes to the working tree.
                if stashed {
                    let _ = git(&wt_for_thread, &["stash", "pop"]);
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
    let _ = store.lock().expect("poisoned").clear_guardian_summary(id);
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
pub(crate) fn worktree_dir(git_root: &str, guardian_id: &str) -> PathBuf {
    Path::new(git_root)
        .join(".ralphus_guardian")
        .join(guardian_id)
}

/// Best-effort removal of every review worktree/branch a guardian created, used
/// when the guardian is deleted. All git operations are ignored on failure (a
/// bare/fake `git_root`, e.g. in tests, simply removes nothing).
pub fn purge_worktrees(git_root: &str, id: &str) {
    let num = id.replace("guardian-", "");
    let wt_base = worktree_dir(git_root, id);
    cleanup_review_worktrees(Path::new(git_root), &wt_base, id, &num, &[]);
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
    if matches!(guardian.status.as_str(), "merging" | "in_review") {
        return reply(
            409,
            &error_body(
                "already_in_progress",
                "a rebase is already in progress; cancel it before starting a new one",
            ),
        );
    }
    if guardian.branches.is_empty() {
        return reply(
            400,
            &error_body("no_branches", "guardian has no branches to merge"),
        );
    }

    let sid = id.to_string();
    std::thread::spawn(move || {
        let _permit = sem.acquire();
        run_merge(&store, runner.as_ref(), &sid);
    });
    reply(202, "{\"status\":\"merging\"}")
}

/// Validate that a branch position has a review worktree, then kick off a
/// background feedback application. Returns immediately.
pub fn start_feedback(
    store: Arc<Mutex<Store>>,
    runner: Arc<dyn Runner>,
    id: &str,
    position: i64,
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
    match guardian.branches.iter().find(|b| b.position == position) {
        Some(b) if b.worktree.is_some() => {}
        Some(_) => {
            return reply(
                409,
                &error_body("not_ready", "run the review merge before giving feedback"),
            );
        }
        None => return reply(404, &error_body("not_found", "no such branch position")),
    }
    let sid = id.to_string();
    std::thread::spawn(move || run_feedback(&store, runner.as_ref(), &sid, position, &feedback));
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
pub fn run_chat(store: &Arc<Mutex<Store>>, runner: Arc<dyn Runner>, id: &str, message: &str) {
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

    // Fetch the full thread. `start_chat` already persisted the latest reviewer
    // message before spawning this thread, so the history ends with it — we do
    // not need to append it separately (doing so was a double-send bug in the
    // original flat-prompt approach).
    let history = store
        .lock()
        .expect("poisoned")
        .guardian_messages(id)
        .unwrap_or_default();

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
        })
        .collect();

    // Try a direct HTTP call first (no subprocess). Falls back to the subprocess
    // runner for unsupported agent types (e.g. claude-code, harness backends).
    let raw_reply = match crate::chat_client::call_direct(
        &r_agent,
        r_model.as_deref(),
        &system,
        &chat_messages,
    ) {
        Ok(text) => text,
        Err(_) => {
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
            let prompt =
                format!("{system}\n\nConversation so far:\n{transcript}\n\nReviewer: {message}");
            let spec = RunnerSpec {
                run_id: "guardian".to_string(),
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
                verify: false,
            };
            let result = runner.run(&spec);
            if result.is_done() && !result.summary.trim().is_empty() {
                result.summary
            } else {
                result
                    .error
                    .unwrap_or_else(|| "the triage agent produced no reply".to_string())
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

    let _ = store
        .lock()
        .expect("poisoned")
        .add_guardian_message(id, "guardian", &visible_text);

    // Dispatch each route block to a fresh agent in the target review worktree,
    // wait for all agents to finish, then restack downstream branches.
    if !routes.is_empty() {
        let no_commit = is_no_commit_intent(message);
        dispatch_routes(store, runner, id, &routes, &guardian, no_commit);
    }
}

/// Append the reviewer's message to the thread and kick off the triage agent's
/// reply in the background. Returns immediately (RAL-22).
pub fn start_chat(
    store: Arc<Mutex<Store>>,
    runner: Arc<dyn Runner>,
    id: &str,
    message: String,
) -> Reply {
    if let Err(e) = store.lock().expect("poisoned").get_guardian(id) {
        return reply(404, &error_body("not_found", &e.to_string()));
    }
    // Persist the reviewer's message synchronously so the UI shows it at once.
    if let Err(e) = store
        .lock()
        .expect("poisoned")
        .add_guardian_message(id, "reviewer", &message)
    {
        return reply(500, &error_body("internal", &e.to_string()));
    }
    let sid = id.to_string();
    std::thread::spawn(move || run_chat(&store, runner, &sid, &message));
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
    let num = id.replace("guardian-", "");
    let base = guardian.base_branch.clone();

    let set_status = |s: GuardianStatus, detail: Option<&str>| {
        let _ = store
            .lock()
            .expect("poisoned")
            .set_guardian_status(id, s, detail);
    };
    set_status(GuardianStatus::Merging, None);
    {
        let guard = store.lock().expect("poisoned");
        let _ = guard.clear_guardian_summary(id);
        let _ = guard.clear_guardian_manual_commands(id);
        let _ = guard.set_guardian_conflicts(id, None, None);
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
            .reset_branch_to_pending(id, ob.position);
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
    // Clean up prior worktrees for ALL projects before starting fresh.
    for (proj, _) in &project_branches {
        let root = PathBuf::from(proj);
        let wt_base = worktree_dir(proj, id);
        cleanup_review_worktrees(&root, &wt_base, id, &num, &[]);
        let _ = std::fs::create_dir_all(&wt_base);
    }

    // Track the last combined worktree across all projects (used for final checks).
    let mut last_combined: Option<String> = None;

    for (proj, proj_branches) in &project_branches {
        let root = PathBuf::from(proj);
        let wt_base = worktree_dir(proj, id);

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
            let ordered: Vec<crate::guardian::OrderedBranch> = proj_branches
                .iter()
                .map(|b| crate::guardian::OrderedBranch {
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
                &set_status,
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
            continue;
        }

        // Per-branch worktree path: stack each branch on top of the previous.
        let mut prev_ref = base_sha.clone();
        for ob in proj_branches {
            let _ = store.lock().expect("poisoned").set_branch_status(
                id,
                ob.position,
                MergeStatus::InProgress,
                None,
            );
            let rev = format!("guardian/{id}/wt-{}", ob.branch);
            let wt = wt_base.join(format!("wt-{}", ob.branch));
            let wt_str = wt.to_string_lossy().to_string();
            if let Err(e) = worktree_add_or_reset(&root, &rev, &wt, &ob.branch) {
                fail_branch(store, id, ob.position, &ob.branch, &e, &set_status);
                return;
            }
            if stack_pick(
                store,
                runner,
                id,
                ob.position,
                &ob.branch,
                &base_sha,
                &prev_ref,
                &rev,
                &wt,
            )
            .is_err()
            {
                return;
            }
            if let Err(e) = run_commit_checks(store, id, &wt, &ob.branch) {
                fail_branch(store, id, ob.position, &ob.branch, &e, &set_status);
                return;
            }
            let _ =
                store
                    .lock()
                    .expect("poisoned")
                    .set_branch_review(id, ob.position, &rev, &wt_str);
            prev_ref = rev;
        }

        // Build per-project combined worktree at the top of this project's stack.
        match rebuild_combined(store, &root, &wt_base, id, &prev_ref) {
            Ok(combined_str) => {
                let combined_wt = std::path::PathBuf::from(&combined_str);
                last_combined = Some(combined_str);
                // RAL-53: generate summary from commit subject lines once the
                // full stack is assembled.
                let completed: Vec<(i64, String)> = proj_branches
                    .iter()
                    .map(|ob| (ob.position, ob.branch.clone()))
                    .collect();
                generate_summary(
                    store, runner, id, &root, &base_sha, &completed, &prev_ref, true,
                );
                // RAL-27: regenerate manual review commands once the stack is ready.
                generate_manual_commands(
                    store,
                    runner,
                    id,
                    &root,
                    &base_sha,
                    &prev_ref,
                    Some(&combined_wt),
                );
            }
            Err(e) => {
                set_status(GuardianStatus::MergeFailed, Some(&e));
                return;
            }
        }
    }

    // Run final check gates against the last combined worktree (all-projects pass).
    let note = if let Some(ref combined_str) = last_combined {
        match final_checks(store, id, combined_str) {
            Ok(n) => n,
            Err(e) => {
                set_status(GuardianStatus::MergeFailed, Some(&e));
                return;
            }
        }
    } else {
        None
    };
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
    root: &Path,
    wt_base: &Path,
    base_sha: &str,
    branches: &[crate::guardian::OrderedBranch],
    set_status: &F,
) {
    let combined_branch = format!("guardian/{id}/review");
    let wt_name = format!("{id}-review");
    let wt = wt_base.join(&wt_name);
    let wt_str = wt.to_string_lossy().to_string();
    if let Err(e) = worktree_add_or_reset(root, &combined_branch, &wt, base_sha) {
        set_status(GuardianStatus::MergeFailed, Some(&e));
        return;
    }
    let mut completed: Vec<(i64, String)> = Vec::new();
    for ob in branches {
        let _ = store.lock().expect("poisoned").set_branch_status(
            id,
            ob.position,
            MergeStatus::InProgress,
            None,
        );
        // Detach at the feature tip and rebase its own commits onto the current
        // combined head; then advance the combined branch to the result.
        if let Err(e) = git(&wt, &["checkout", "--detach", &ob.branch]) {
            let _ = git(&wt, &["checkout", "--force", &combined_branch]);
            fail_branch(store, id, ob.position, &ob.branch, &e, set_status);
            return;
        }
        match drive_rebase(
            store,
            id,
            ob.position,
            runner,
            &ob.branch,
            &wt,
            &combined_branch,
            base_sha,
            "HEAD",
        ) {
            Ok(outcome) => {
                let nothing = contributed_nothing(&wt, &combined_branch, "HEAD");
                if let Err(e) = git(&wt, &["checkout", "-B", &combined_branch, "HEAD"]) {
                    fail_branch(store, id, ob.position, &ob.branch, &e, set_status);
                    return;
                }
                let (status, detail) = match outcome {
                    RebaseOutcome::Resolved => {
                        (MergeStatus::ConflictResolved, Some("resolved by agent"))
                    }
                    RebaseOutcome::Clean => (
                        MergeStatus::Done,
                        nothing.then_some("no new commits over base (already merged?)"),
                    ),
                };
                let _ = store.lock().expect("poisoned").set_branch_status(
                    id,
                    ob.position,
                    status,
                    detail,
                );
            }
            Err(e) => {
                // drive_rebase already aborted; restore the combined branch.
                let _ = git(&wt, &["checkout", "--force", &combined_branch]);
                fail_branch(store, id, ob.position, &ob.branch, &e, set_status);
                return;
            }
        }
        if let Err(e) = run_commit_checks(store, id, &wt, &ob.branch) {
            fail_branch(store, id, ob.position, &ob.branch, &e, set_status);
            return;
        }
        // Every branch shares the one combined worktree/branch.
        let _ = store.lock().expect("poisoned").set_branch_review(
            id,
            ob.position,
            &combined_branch,
            &wt_str,
        );
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
    match final_checks(store, id, &wt_str) {
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
            // RAL-27: generate manual review commands once the stack is ready.
            generate_manual_commands(
                store,
                runner,
                id,
                root,
                base_sha,
                &combined_branch,
                Some(&wt),
            );
            set_status(GuardianStatus::InReview, note.as_deref());
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
    position: i64,
    feedback: &str,
) {
    let guardian = match store.lock().expect("poisoned").get_guardian(id) {
        Ok(g) => g,
        Err(_) => return,
    };
    let base = guardian.base_branch.clone();

    let set_status = |s: GuardianStatus, detail: Option<&str>| {
        let _ = store
            .lock()
            .expect("poisoned")
            .set_guardian_status(id, s, detail);
    };

    let Some(branch) = guardian.branches.iter().find(|b| b.position == position) else {
        return;
    };
    // Resolve this branch's effective project root (RAL-29: may differ from primary).
    let branch_project = branch
        .project
        .clone()
        .unwrap_or_else(|| guardian.git_root.clone());
    let root = PathBuf::from(&branch_project);
    let wt_base = worktree_dir(&branch_project, id);

    let Some(wt_str) = branch.worktree.clone() else {
        set_status(
            GuardianStatus::MergeFailed,
            Some("no review worktree yet; run the merge first"),
        );
        return;
    };
    let feature = branch.branch.clone();
    let wt = PathBuf::from(&wt_str);
    set_status(GuardianStatus::Merging, None);
    let _ = store.lock().expect("poisoned").clear_guardian_summary(id);

    // The agent edits the review worktree; we commit onto its review branch.
    let prompt = format!(
        "You are revising branch '{feature}' in response to reviewer feedback. \
         Edit the files in this worktree to satisfy the feedback, then stop. \
         Feedback: {feedback}. Do not run any git commands."
    );
    let r_agent = resolver_agent(guardian.resolver_agent.as_deref());
    let r_model = resolver_model(guardian.resolver_model.as_deref(), &r_agent);
    let spec = RunnerSpec {
        run_id: "guardian".to_string(),
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
        verify: false,
    };
    let no_commit = is_no_commit_intent(feedback);
    // Stash any pre-existing dirty state so we only include the agent's own
    // changes in the new commit (not leftovers from a prior no-commit turn).
    let stashed = if !no_commit {
        let pre = git(&wt, &["status", "--porcelain"]).unwrap_or_default();
        if !pre.trim().is_empty() {
            git(&wt, &["stash", "--include-untracked"]).is_ok()
        } else {
            false
        }
    } else {
        false
    };
    let _ = runner.run(&spec);
    let dirty = git(&wt, &["status", "--porcelain"]).unwrap_or_default();
    if !dirty.trim().is_empty() && !no_commit {
        let _ = git(&wt, &["add", "-A"]);
        let _ = git(
            &wt,
            &["commit", "-m", &format!("review feedback: {feedback}")],
        );
    }
    // Restore any pre-existing (no-commit) changes to the working tree.
    if stashed {
        let _ = git(&wt, &["stash", "pop"]);
    }
    let _ = store
        .lock()
        .expect("poisoned")
        .set_branch_detail(id, position, "feedback applied");
    if no_commit {
        set_status(GuardianStatus::InReview, None);
        return;
    }

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
    // Downstream branches in the same project, in position order.
    let downstream: Vec<_> = all_branches
        .iter()
        .filter(|b| {
            b.position > position
                && b.project.as_deref().unwrap_or(&guardian.git_root) == branch_project
        })
        .collect();
    let mut prev_ref = branch
        .review_branch
        .clone()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| format!("guardian/{id}/wt-{}", branch.branch));
    for ob in &downstream {
        let _ = store.lock().expect("poisoned").set_branch_status(
            id,
            ob.position,
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
        let wt_j_str = wt_j.to_string_lossy().to_string();
        // Reset the review branch to the feature tip; drive_rebase replays its
        // own commits onto the revised upstream (`prev_ref`).
        if let Err(e) = worktree_add_or_reset(&root, &rev, &wt_j, &ob.branch) {
            fail_branch(store, id, ob.position, &ob.branch, &e, &set_status);
            return;
        }
        if stack_pick(
            store,
            runner,
            id,
            ob.position,
            &ob.branch,
            &base_sha,
            &prev_ref,
            &rev,
            &wt_j,
        )
        .is_err()
        {
            return;
        }
        if let Err(e) = run_commit_checks(store, id, &wt_j, &ob.branch) {
            fail_branch(store, id, ob.position, &ob.branch, &e, &set_status);
            return;
        }
        let _ = store
            .lock()
            .expect("poisoned")
            .set_branch_review(id, ob.position, &rev, &wt_j_str);
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
            // RAL-27: regenerate manual review commands after the re-stack.
            generate_manual_commands(
                store,
                runner,
                id,
                &root,
                &base_sha,
                &prev_ref,
                Some(&wt_base.join(format!("{id}-review"))),
            );
            set_status(GuardianStatus::InReview, note.as_deref());
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
            let runner: Arc<dyn Runner> = Arc::new(crate::runner::SubprocessRunner::from_env());
            rebuild_on_base_shift(&store, runner.as_ref(), &id, &sem);
        });
    }
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
        let current = match resolve_base(Path::new(proj), &guardian.base_branch) {
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
    feature_branch: &str,
    base_sha: &str,
    newbase: &str,
    rev: &str,
    wt: &Path,
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
        runner,
        feature_branch,
        wt,
        newbase,
        base_sha,
        rev,
    ) {
        Ok(outcome) => {
            let (status, detail) = match outcome {
                RebaseOutcome::Resolved => {
                    (MergeStatus::ConflictResolved, Some("resolved by agent"))
                }
                RebaseOutcome::Clean => (
                    MergeStatus::Done,
                    // Surface a branch that added nothing over the base rather than
                    // reporting a silent, work-free "done".
                    contributed_nothing(wt, newbase, rev)
                        .then_some("no new commits over base (already merged?)"),
                ),
            };
            let _ = store
                .lock()
                .expect("poisoned")
                .set_branch_status(id, position, status, detail);
            Ok(())
        }
        Err(e) => {
            fail_branch(store, id, position, feature_branch, &e, &set_status);
            Err(())
        }
    }
}

/// Rebuild the combined review worktree at `prev_ref` and run the check gates.
///
/// Returns an optional informational note for the `InReview` status: `Some(...)`
/// when the check gates were opted out (CCTL-130), so the UI can distinguish
/// "build skipped" from "build passed"; `None` when checks ran and passed.
fn finalize_review(
    store: &Arc<Mutex<Store>>,
    root: &Path,
    wt_base: &Path,
    id: &str,
    prev_ref: &str,
) -> std::result::Result<Option<String>, String> {
    let combined_str = rebuild_combined(store, root, wt_base, id, prev_ref)?;
    final_checks(store, id, &combined_str)
}

/// Run the review's check gates against the finished combined worktree. Returns
/// `Some(note)` when checks were opted out (so the UI shows "skipped" vs
/// "passed"), `None` when they ran and passed, or `Err` on the first failure.
fn final_checks(
    store: &Arc<Mutex<Store>>,
    id: &str,
    combined_str: &str,
) -> std::result::Result<Option<String>, String> {
    let (skip, checks) = {
        let guard = store.lock().expect("poisoned");
        (
            guard.guardian_skip_checks(id).unwrap_or(false),
            guard.guardian_checks(id).unwrap_or_default(),
        )
    };
    if skip {
        return Ok((!checks.is_empty()).then(|| "check gates skipped (opt-out)".to_string()));
    }
    for cmd in &checks {
        if !crate::verify::run_command_verify(combined_str, cmd) {
            return Err(format!("check failed: {cmd}"));
        }
    }
    Ok(None)
}

/// (Re)create the stable, read-only combined worktree at `prev_ref`, pointing at
/// the `review` branch (the head of the full stacked review).
fn rebuild_combined(
    store: &Arc<Mutex<Store>>,
    root: &Path,
    wt_base: &Path,
    id: &str,
    prev_ref: &str,
) -> std::result::Result<String, String> {
    let combined_branch = format!("guardian/{id}/review");
    let wt_name = format!("{id}-review");
    let combined_wt = wt_base.join(&wt_name);
    let combined_str = combined_wt.to_string_lossy().to_string();
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
    root: &Path,
    wt_base: &Path,
    id: &str,
    num: &str,
    old_review_branches: &[String],
) {
    let list = git(root, &["worktree", "list", "--porcelain"]).unwrap_or_default();
    for line in list.lines() {
        if let Some(path) = line.strip_prefix("worktree ") {
            let path = path.trim();
            if path.contains(id) {
                let _ = git(root, &["worktree", "remove", "--force", path]);
            }
        }
    }
    let _ = git(root, &["worktree", "prune"]);
    let _ = std::fs::remove_dir_all(wt_base);
    for branch in old_review_branches {
        let _ = git(root, &["branch", "-D", branch]);
    }
    let refs = git(
        root,
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
        let _ = git(root, &["branch", "-D", branch]);
    }
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

/// How a branch's commits landed on the stack.
enum RebaseOutcome {
    /// Rebased with no conflicts.
    Clean,
    /// Rebased after the agent resolved conflicts.
    Resolved,
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
pub(crate) fn resolve_base(root: &Path, base_branch: &str) -> std::result::Result<String, String> {
    let spec = format!("{base_branch}^{{commit}}");
    Ok(git(root, &["rev-parse", "--verify", &spec])?
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
    runner: &dyn Runner,
    feature: &str,
    wt: &Path,
    newbase: &str,
    base_sha: &str,
    branch_arg: &str,
) -> std::result::Result<RebaseOutcome, String> {
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
    match git(wt, &args) {
        Ok(_) => Ok(RebaseOutcome::Clean),
        Err(e) => {
            if conflicted_files(wt).is_empty() {
                // Failed with no conflict to resolve (e.g. a bad ref): abort clean.
                let _ = git(wt, &["rebase", "--abort"]);
                Err(e)
            } else {
                match resolve_conflicts_with_agent(store, id, position, runner, wt, feature) {
                    Ok(()) => Ok(RebaseOutcome::Resolved),
                    Err(re) => {
                        let _ = git(wt, &["rebase", "--abort"]);
                        Err(re)
                    }
                }
            }
        }
    }
}

/// Whether a just-built review branch (`rev`) contributed no commits over
/// `newbase` — i.e. all of the feature's changes were already present. Returned
/// as a branch detail so a silently-empty stack entry is surfaced, not hidden.
fn contributed_nothing(wt: &Path, newbase: &str, rev: &str) -> bool {
    let range = format!("{newbase}..{rev}");
    git(wt, &["rev-list", "--count", &range])
        .ok()
        .and_then(|c| c.trim().parse::<i64>().ok())
        .is_some_and(|n| n == 0)
}

/// Generate the cross-branch change summary for a guardian using the resolver
/// agent. Called after the stack is fully assembled (`is_final=true` organises
/// output per-branch when individual refs exist; otherwise uses the combined log).
///
/// The result is stored as `change_summary` on the guardian and surfaced in the
/// review detail pane. Failures are silent — a missing summary is better than a
/// crashed merge thread.
///
/// RAL-53: uses commit subject lines only (no diffs) so the output describes
/// developer intent rather than low-level file changes.
#[allow(clippy::too_many_arguments)]
fn generate_summary(
    store: &Arc<Mutex<Store>>,
    runner: &dyn Runner,
    id: &str,
    root: &Path,
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
        let has = first_ref.is_some_and(|r| git(root, &["rev-parse", "--verify", &r]).is_ok());
        if has {
            let mut prev = base_sha.to_string();
            let mut lines: Vec<String> = Vec::new();
            for (_, branch_name) in completed {
                let branch_ref = format!("guardian/{id}/wt-{branch_name}");
                let b_log = git(
                    root,
                    &["log", "--format=%s", &format!("{prev}..{branch_ref}")],
                )
                .unwrap_or_default();
                if !b_log.trim().is_empty() {
                    lines.push(format!("{branch_name}:\n{b_log}"));
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

    let branch_list = completed
        .iter()
        .map(|(_, n)| n.as_str())
        .collect::<Vec<_>>()
        .join(", ");

    let context = if has_per_branch_refs {
        context
    } else {
        let log = git(
            root,
            &["log", "--format=%s", &format!("{base_sha}..{tip_ref}")],
        )
        .unwrap_or_default();
        if log.trim().is_empty() {
            return;
        }
        format!("Branches: [{branch_list}]\n\n{log}")
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
    let cwd = root.to_string_lossy().into_owned();

    let prompt = format!(
        "You are summarising a stacked code review. The following are commit \
         subject lines for branches [{branch_list}] — one line per commit. \
         Write a compact 2-3 sentence summary in plain language describing \
         what was changed and why. Focus on developer intent, not file-level \
         details.\n\n{context}"
    );
    let spec = RunnerSpec {
        run_id: "guardian".to_string(),
        task: "summary".to_string(),
        session_id: "summarizer".to_string(),
        cwd,
        prompt: Some(prompt),
        command: None,
        agent,
        model,
        system_prompt: None,
        system_prompt_position: None,
        timeout_sec: None,
        budget_tokens: None,
        verify: false,
    };
    let result = runner.run(&spec);
    if result.is_done() && !result.summary.trim().is_empty() {
        let _ = store
            .lock()
            .expect("poisoned")
            .set_guardian_summary(id, &result.summary);
    }
}

/// Generate LLM-suggested shell commands for manually testing or verifying the
/// changes in the review branch (RAL-27). Asks the resolver agent to propose
/// 1-5 hands-on human verification steps, and persists the result. Failures are
/// silent — a missing command list is better than a crash.
///
/// `worktree` is the combined review worktree path (preferred). When present the
/// LLM runs inside the worktree with its `run_bash` tool so it can inspect the
/// diff itself — no diff content is embedded in the prompt, which avoids OS
/// command-line length limits in harness backends. Falls back to a file-name
/// list from `root` when no worktree is available.
fn generate_manual_commands(
    store: &Arc<Mutex<Store>>,
    runner: &dyn Runner,
    id: &str,
    root: &Path,
    base_sha: &str,
    tip_ref: &str,
    worktree: Option<&Path>,
) {
    let (cwd, prompt) = if let Some(wt) = worktree {
        // Worktree path: embed only the --stat output (always compact — one line
        // per changed file). Never embed the full diff; it can be arbitrarily
        // large and would blow OS command-line limits in harness backends.
        let stat = git(wt, &["diff", "--stat", base_sha]).unwrap_or_default();
        if stat.trim().is_empty() {
            return;
        }
        let log = git(
            root,
            &["log", "--format=%s", &format!("{base_sha}..{tip_ref}")],
        )
        .unwrap_or_default();
        let p = format!(
            "You are preparing a code review. Based on the changed files and commit \
             messages below, produce a JSON array of 1-5 shell command strings that a \
             human reviewer should run to manually verify these changes. Focus on \
             hands-on, observable steps: launching the app and inspecting it visually, \
             running a build script, or exercising a CLI feature by hand. Do NOT \
             suggest unit tests or automated checks that could be scripted — the goal \
             is human eyes and hands on the actual result. \
             Return ONLY a valid JSON array of strings — no markdown fences, no \
             explanation, no other text.\n\n\
             Changed files (stat):\n{stat}\n\n\
             Commit messages:\n{log}"
        );
        (wt.to_string_lossy().into_owned(), p)
    } else {
        // Fallback: list changed file names from the repository root. The file
        // list is always small, so it is safe to embed directly.
        let files = match git(
            root,
            &["diff", "--name-only", &format!("{base_sha}..{tip_ref}")],
        ) {
            Ok(s) if !s.trim().is_empty() => s,
            _ => return,
        };
        let log = git(
            root,
            &["log", "--format=%s", &format!("{base_sha}..{tip_ref}")],
        )
        .unwrap_or_default();
        let p = format!(
            "You are preparing a code review. Based on the changed files and commit \
             messages below, produce a JSON array of 1-5 shell command strings that a \
             human reviewer should run to manually verify these changes. Focus on \
             hands-on, observable steps: launching the app and inspecting it visually, \
             running a build script, or exercising a CLI feature by hand. Do NOT \
             suggest unit tests or automated checks that could be scripted — the goal \
             is human eyes and hands on the actual result. \
             Return ONLY a valid JSON array of strings — no markdown fences, no \
             explanation, no other text.\n\n\
             Changed files:\n{files}\n\n\
             Commit messages:\n{log}"
        );
        (root.to_string_lossy().into_owned(), p)
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
        run_id: "guardian".to_string(),
        task: "manual_commands".to_string(),
        session_id: "manual-reviewer".to_string(),
        cwd,
        prompt: Some(prompt),
        command: None,
        agent,
        model,
        system_prompt: None,
        system_prompt_position: None,
        timeout_sec: None,
        budget_tokens: None,
        verify: false,
    };

    let result = runner.run(&spec);
    if !result.is_done() || result.summary.trim().is_empty() {
        return;
    }

    let text = result.summary.trim();
    let commands: Vec<String> = if let Ok(arr) = serde_json::from_str::<Vec<String>>(text) {
        arr
    } else if let Some(start) = text.find('[') {
        let end = text.rfind(']').unwrap_or(text.len().saturating_sub(1));
        serde_json::from_str::<Vec<String>>(&text[start..=end]).unwrap_or_default()
    } else {
        return;
    };

    if !commands.is_empty() {
        let _ = store
            .lock()
            .expect("poisoned")
            .set_guardian_manual_commands(id, &commands);
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
}
