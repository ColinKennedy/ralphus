//! Proof-step execution.
//!
//! `command` proof steps (fmt/lint/test) run as shell commands — exit code
//! is the verdict. `agent` provers need a model; `brain` and `approval`
//! provers are deferred (left pending) until their respective backends
//! land.
//!
//! This module's direct, un-wrapped subprocess execution is used by Guardian
//! review check gates (`daemon/src/guardian_merge.rs`), which have no live
//! view of their own to preserve. A task/cell `command`-kind proof step
//! instead runs through [`crate::runner::Runner`] (RAL-151), tmux-wrapped
//! exactly like a `prompt`-kind proof step, so it can be watched live from
//! the board — see `daemon/src/scheduler.rs::run_proofs`'s `"command"`
//! branch.

use std::collections::BTreeMap;
use std::process::{Command, Stdio};

use opentelemetry::Context;
use opentelemetry::trace::{SpanKind, Status};

/// Run a `command` proof step in `cwd`. Returns true when the command exits 0.
///
/// Used by callers (e.g. Guardian review checks) that have no trace to
/// continue and no squad-scoped env overrides to apply; the span it creates
/// starts a fresh trace of its own.
#[must_use]
pub fn run_command_proof(cwd: &str, command: &str) -> bool {
    run_command_proof_capture(cwd, command, &Context::new(), &BTreeMap::new()).0
}

/// Like [`run_command_proof`] but also captures the combined stdout+stderr
/// (truncated) so it can be shown in the log viewer (CCTL-99). Wraps the
/// subprocess in an OpenTelemetry span (RAL-96), as a child of `parent`, and
/// applies `env` — the owning squad's persistent environment-variable overrides
/// (RAL-150), if any — on top of the daemon's own inherited environment.
#[must_use]
pub fn run_command_proof_capture(
    cwd: &str,
    command: &str,
    parent: &Context,
    env: &BTreeMap<String, String>,
) -> (bool, String) {
    let span = crate::otel::start_span("proof.command", parent, SpanKind::Internal);
    span.set_attribute("proof.cwd", cwd.to_string());
    span.set_attribute("proof.command", command.to_string());
    crate::rlog!(
        DEBUG,
        "ralphus [proof] command starting cwd={cwd:?} command={command:?}"
    );
    let (shell, flag) = if cfg!(windows) {
        ("cmd", "/C")
    } else {
        ("sh", "-c")
    };
    let result = match Command::new(shell)
        .arg(flag)
        .arg(command)
        .current_dir(cwd)
        .envs(env)
        .stdin(Stdio::null())
        .output()
    {
        Ok(out) => {
            let mut buf = String::new();
            buf.push_str(&String::from_utf8_lossy(&out.stdout));
            let err = String::from_utf8_lossy(&out.stderr);
            if !err.trim().is_empty() {
                buf.push_str(&err);
            }
            (out.status.success(), truncate_output(&buf))
        }
        Err(e) => (false, format!("could not run command: {e}")),
    };
    span.set_status(if result.0 {
        Status::Ok
    } else {
        Status::error("proof command failed")
    });
    crate::rlog!(
        DEBUG,
        "ralphus [proof] command completed passed={} cwd={cwd:?}",
        result.0
    );
    result
}

/// Resolve the current `HEAD` commit sha of `cwd`, or `None` if `cwd` isn't a
/// git working tree, has no commits yet, or `git` isn't available. Used by the
/// finalizer's no-new-commits guard (RAL-156): to capture a git-backed task's
/// baseline at cell start, and to compare each cell's `cwd` against it
/// at finalize time.
#[must_use]
pub fn git_head_sha(cwd: &str) -> Option<String> {
    let out = Command::new("git")
        .arg("rev-parse")
        .arg("HEAD")
        .current_dir(cwd)
        .stdin(Stdio::null())
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let sha = String::from_utf8_lossy(&out.stdout).trim().to_string();
    (!sha.is_empty()).then_some(sha)
}

/// Whether any of `cwds` has moved its `HEAD` past `baseline` (RAL-156): the
/// deterministic "did this task produce at least one commit" check the
/// finalizer runs for git-backed tasks, in place of trusting the agent's own
/// self-report. Fails closed per-`cwd` — one whose `HEAD` can't be resolved
/// (bad path, not a git repo, no commits) counts as "no new commit" for that
/// cell, never as a pass.
#[must_use]
pub fn any_cwd_has_new_commit(cwds: &[&str], baseline: &str) -> bool {
    cwds.iter()
        .any(|cwd| git_head_sha(cwd).is_some_and(|head| head != baseline))
}

/// Cap captured output so a runaway verifier can't bloat the DB / UI.
fn truncate_output(s: &str) -> String {
    const MAX: usize = 16 * 1024;
    if s.len() <= MAX {
        return s.to_string();
    }
    let mut cut = MAX;
    while !s.is_char_boundary(cut) {
        cut -= 1;
    }
    format!("{}\n…(truncated)", &s[..cut])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn passing_command_is_true() {
        assert!(run_command_proof(".", "exit 0"));
    }

    #[test]
    fn failing_command_is_false() {
        assert!(!run_command_proof(".", "exit 1"));
    }

    #[test]
    fn bad_cwd_is_false() {
        assert!(!run_command_proof("/no/such/dir/ralphus-xyz", "exit 0"));
    }

    #[test]
    fn capture_returns_output() {
        let (ok, out) =
            run_command_proof_capture(".", "echo hello-proof", &Context::new(), &BTreeMap::new());
        assert!(ok);
        assert!(out.contains("hello-proof"), "captured: {out:?}");
    }

    #[test]
    fn capture_applies_env_overrides() {
        let mut env = BTreeMap::new();
        env.insert(
            "RALPHUS_TEST_VAR".to_string(),
            "hello-env-override".to_string(),
        );
        let cmd = if cfg!(windows) {
            "echo %RALPHUS_TEST_VAR%"
        } else {
            "echo $RALPHUS_TEST_VAR"
        };
        let (ok, out) = run_command_proof_capture(".", cmd, &Context::new(), &env);
        assert!(ok);
        assert!(out.contains("hello-env-override"), "captured: {out:?}");
    }

    #[test]
    fn truncate_caps_huge_output() {
        let big = "x".repeat(40_000);
        let out = truncate_output(&big);
        assert!(out.len() <= 16 * 1024 + 32, "not truncated: {}", out.len());
        assert!(out.ends_with("…(truncated)"));
        // Small output is returned verbatim.
        assert_eq!(truncate_output("short"), "short");
    }

    // ── RAL-156: no-new-commits guard helpers ────────────────────────────────

    use std::sync::atomic::{AtomicU32, Ordering};

    static TEST_N: AtomicU32 = AtomicU32::new(0);

    fn tmp_dir(tag: &str) -> std::path::PathBuf {
        let n = TEST_N.fetch_add(1, Ordering::Relaxed);
        let dir =
            std::env::temp_dir().join(format!("ral156-proof-{tag}-{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("mkdir tmp_dir");
        dir
    }

    fn g(root: &std::path::Path, args: &[&str]) {
        let status = Command::new("git")
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
    fn init_repo(tag: &str) -> std::path::PathBuf {
        let repo = tmp_dir(tag);
        g(&repo, &["init", "-b", "main"]);
        std::fs::write(repo.join("base.txt"), "base\n").unwrap();
        g(&repo, &["add", "."]);
        g(&repo, &["commit", "-m", "base"]);
        repo
    }

    #[test]
    fn git_head_sha_resolves_in_a_real_repo() {
        let repo = init_repo("head");
        let sha = git_head_sha(&repo.to_string_lossy()).expect("HEAD sha");
        assert_eq!(sha.len(), 40, "expected a full sha: {sha:?}");
    }

    #[test]
    fn git_head_sha_none_for_non_repo() {
        let dir = tmp_dir("not-a-repo");
        assert!(git_head_sha(&dir.to_string_lossy()).is_none());
    }

    #[test]
    fn git_head_sha_none_for_bad_cwd() {
        assert!(git_head_sha("/no/such/dir/ralphus-xyz").is_none());
    }

    #[test]
    fn any_cwd_has_new_commit_false_when_head_unchanged() {
        let repo = init_repo("unchanged");
        let baseline = git_head_sha(&repo.to_string_lossy()).expect("HEAD sha");
        let cwd = repo.to_string_lossy().to_string();
        assert!(!any_cwd_has_new_commit(&[&cwd], &baseline));
    }

    #[test]
    fn any_cwd_has_new_commit_true_after_a_commit() {
        let repo = init_repo("changed");
        let baseline = git_head_sha(&repo.to_string_lossy()).expect("HEAD sha");
        std::fs::write(repo.join("more.txt"), "more\n").unwrap();
        g(&repo, &["add", "."]);
        g(&repo, &["commit", "-m", "more"]);
        let cwd = repo.to_string_lossy().to_string();
        assert!(any_cwd_has_new_commit(&[&cwd], &baseline));
    }

    #[test]
    fn any_cwd_has_new_commit_true_when_any_of_several_changed() {
        // Two cells of the same task: the first's cwd never moved past the
        // baseline; the second's got an extra commit on top -- standing in for
        // "this cell made progress since the task started." (Deliberately
        // *not* two independent single-commit repos: with identical tree,
        // message, and author/committer env, two repos created in the same
        // second can hash to the same commit sha, making the "changed" repo
        // indistinguishable from the baseline by fluke.)
        let unchanged = init_repo("multi-unchanged");
        let baseline = git_head_sha(&unchanged.to_string_lossy()).expect("HEAD sha");
        let changed = init_repo("multi-changed");
        std::fs::write(changed.join("more.txt"), "more\n").unwrap();
        g(&changed, &["add", "."]);
        g(&changed, &["commit", "-m", "more"]);
        let unchanged_cwd = unchanged.to_string_lossy().to_string();
        let changed_cwd = changed.to_string_lossy().to_string();
        assert!(any_cwd_has_new_commit(
            &[&unchanged_cwd, &changed_cwd],
            &baseline
        ));
    }

    #[test]
    fn any_cwd_has_new_commit_false_when_no_cwd_resolves() {
        assert!(!any_cwd_has_new_commit(
            &["/no/such/dir/ralphus-xyz"],
            "deadbeef"
        ));
    }
}
