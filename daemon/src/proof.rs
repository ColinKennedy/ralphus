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
    // ralphus[ignore-rlog-pair]: this low-level helper has no Store; its Store-owning caller records the structured workflow outcome
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
    // ralphus[ignore-rlog-pair]: this low-level helper has no Store; its Store-owning caller records the structured workflow outcome
    crate::rlog!(
        DEBUG,
        "ralphus [proof] command completed passed={} cwd={cwd:?}",
        result.0
    );
    result
}

/// Whether any of `cwds` has at least one commit its own `@{upstream}`
/// tracking ref doesn't (RAL-293): the deterministic "did this task produce
/// real, durable progress" check the finalizer runs for git-backed tasks, in
/// place of trusting the agent's own self-report. Delegates the per-`cwd`
/// question to [`crate::reviews::worktree_has_commits_ahead_of_upstream`],
/// which reads live git state rather than a squad-run-scoped baseline sha —
/// so it answers the same way whether the commit was made just now or by an
/// earlier run that reused this same worktree. Fails closed per-`cwd`: one
/// with no resolvable upstream counts as "no progress" for that cell, never
/// as a pass.
#[must_use]
pub fn any_cwd_ahead_of_upstream(cwds: &[&str]) -> bool {
    cwds.iter().any(|cwd| {
        crate::reviews::worktree_has_commits_ahead_of_upstream(std::path::Path::new(cwd))
    })
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

    // ── RAL-293: no-new-commits guard helpers ────────────────────────────────

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

    /// A fresh repo with one commit on `main`, tracking a local `base` branch
    /// pinned to that same commit -- standing in for the `?upstream=` tracking
    /// ref every real worktree materializes (`worktrees::set_explicit_upstream`).
    fn init_repo(tag: &str) -> std::path::PathBuf {
        let repo = tmp_dir(tag);
        g(&repo, &["init", "-b", "main"]);
        std::fs::write(repo.join("base.txt"), "base\n").unwrap();
        g(&repo, &["add", "."]);
        g(&repo, &["commit", "-m", "base"]);
        g(&repo, &["branch", "base"]);
        g(&repo, &["branch", "--set-upstream-to=base", "main"]);
        repo
    }

    #[test]
    fn any_cwd_ahead_of_upstream_false_when_head_equals_upstream() {
        let repo = init_repo("unchanged");
        let cwd = repo.to_string_lossy().to_string();
        assert!(!any_cwd_ahead_of_upstream(&[&cwd]));
    }

    #[test]
    fn any_cwd_ahead_of_upstream_true_after_a_commit() {
        let repo = init_repo("changed");
        std::fs::write(repo.join("more.txt"), "more\n").unwrap();
        g(&repo, &["add", "."]);
        g(&repo, &["commit", "-m", "more"]);
        let cwd = repo.to_string_lossy().to_string();
        assert!(any_cwd_ahead_of_upstream(&[&cwd]));
    }

    #[test]
    fn any_cwd_ahead_of_upstream_true_when_any_of_several_changed() {
        // Two cells of the same task: the first's cwd never moved past its
        // upstream; the second's got an extra commit on top -- standing in
        // for "this cell made progress."
        let unchanged = init_repo("multi-unchanged");
        let changed = init_repo("multi-changed");
        std::fs::write(changed.join("more.txt"), "more\n").unwrap();
        g(&changed, &["add", "."]);
        g(&changed, &["commit", "-m", "more"]);
        let unchanged_cwd = unchanged.to_string_lossy().to_string();
        let changed_cwd = changed.to_string_lossy().to_string();
        assert!(any_cwd_ahead_of_upstream(&[&unchanged_cwd, &changed_cwd]));
    }

    #[test]
    fn any_cwd_ahead_of_upstream_false_when_no_cwd_resolves() {
        assert!(!any_cwd_ahead_of_upstream(&["/no/such/dir/ralphus-xyz"]));
    }

    #[test]
    fn any_cwd_ahead_of_upstream_true_regardless_of_which_run_made_the_commit() {
        // RAL-293's actual repro: the commit already existed before this
        // check ever ran (e.g. made by an earlier squad run that reused this
        // worktree) -- @{upstream} must still see it as progress, unlike the
        // old squad-run-scoped baseline sha which only saw commits made
        // *during* the run capturing it.
        let repo = init_repo("prior-run");
        std::fs::write(repo.join("prior-run.txt"), "already done\n").unwrap();
        g(&repo, &["add", "."]);
        g(&repo, &["commit", "-m", "prior run's work"]);
        let cwd = repo.to_string_lossy().to_string();
        assert!(any_cwd_ahead_of_upstream(&[&cwd]));
    }
}
