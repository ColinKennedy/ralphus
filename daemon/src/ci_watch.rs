//! Watch a PR's CI/CD + mergeability after a review-feedback push (RAL-375).
//!
//! [`watch_after_feedback_push`] is called right after
//! `guardian_merge::run_feedback` pushes a new commit onto a stacked branch:
//! if that branch already has an open forge PR (`crate::pr::PullRequestView`),
//! it spawns a background poll of [`crate::forge::ForgeClient::check_pr_ci_status`]
//! -- fast at first to catch a quick pipeline, backing off for a slow one
//! (see [`next_poll_delay`]) -- until a terminal state. A terminal failure
//! drops a `"review"`-category mailbox notice (`crate::mailbox`) naming the
//! PR, the failing job, the impacted review worktree, and a trimmed log
//! excerpt, then asks whether to fix it immediately in a subagent. A
//! terminal success is silent.
//!
//! There is no new tracking type here: a PR is already reachable from a
//! review worktree via the `guardian_id`/`branch_id` pair
//! `PullRequestView` and `BranchView` share.

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::{Arc, LazyLock, Mutex};
use std::time::{Duration, Instant};

use crate::forge::{PrCiState, PrFailure};
use crate::guardian::{BranchView, GuardianView};
use crate::logging::LogLevel;
use crate::mailbox::MailboxPriority;
use crate::pr::PullRequestView;
use crate::store::{Store, now_ms};

/// Fast-start/backoff poll cadence (RAL-375): quick enough to catch a
/// pipeline that finishes in seconds, but backs off for one that runs many
/// minutes, so the poller doesn't hammer the forge API on the slow end.
/// Table-driven and pure -- no clock or network needed to test it -- unlike
/// `scheduler::FORGE_REORDER_POLL_INTERVAL`, which is a flat interval for an
/// unrelated concern (stack-order drift) and never backs off.
#[must_use]
pub fn next_poll_delay(elapsed_since_start: Duration) -> Duration {
    /// `(elapsed threshold, delay to use below it)`, checked in order. Falls
    /// through to `FALLBACK_DELAY` once `elapsed_since_start` exceeds every
    /// threshold here.
    const STAGES: &[(Duration, Duration)] = &[
        (Duration::from_secs(30), Duration::from_secs(5)),
        (Duration::from_secs(5 * 60), Duration::from_secs(20)),
    ];
    const FALLBACK_DELAY: Duration = Duration::from_secs(60);
    STAGES
        .iter()
        .find(|(threshold, _)| elapsed_since_start < *threshold)
        .map_or(FALLBACK_DELAY, |(_, delay)| *delay)
}

/// Give up watching a PR after this long with no terminal status (RAL-375):
/// a pipeline still not done after this long is either stuck or this daemon
/// missed its completion, and polling forever serves neither case. Generous
/// headroom over the slowest pipelines observed (10+ minutes).
pub const MAX_WATCH_DURATION: Duration = Duration::from_secs(2 * 60 * 60);

/// Do not accept an apparently-green result immediately after a push. Both
/// forges can briefly report a clean mergeability verdict before the new
/// commit's pipeline/check suite has been created; stopping on that first
/// response would miss exactly the fast CI failures this watcher exists to
/// catch. Failures remain terminal immediately.
const SUCCESS_SETTLE_DURATION: Duration = Duration::from_secs(30);

/// How many lines of a captured log to keep from the head and tail when
/// trimming for a mailbox notice (RAL-375) -- enough context to diagnose a
/// failure without carrying a full CI log into the mailbox.
const LOG_EXCERPT_LINES: usize = 60;

/// Head/tail excerpt of a log (RAL-375): keeps the first `head_lines` and
/// last `tail_lines` lines, replacing whatever's between with a count of the
/// omitted lines. Returns `text` unchanged when it already fits the budget.
#[must_use]
pub fn trim_log_excerpt(text: &str, head_lines: usize, tail_lines: usize) -> String {
    let lines: Vec<&str> = text.lines().collect();
    if lines.len() <= head_lines + tail_lines {
        return text.to_string();
    }
    let omitted = lines.len() - head_lines - tail_lines;
    format!(
        "{}\n... [{omitted} line(s) omitted] ...\n{}",
        lines[..head_lines].join("\n"),
        lines[lines.len() - tail_lines..].join("\n"),
    )
}

/// Sanitize a string for use as one path segment: anything that isn't
/// alphanumeric/`-`/`_` becomes `-`. Used only to keep a debug-identifiable
/// temp-directory name; the value is never trusted for traversal since it's
/// always an internal PR/job id, not external input.
fn sanitize_path_segment(s: &str) -> String {
    s.chars()
        .map(|c| {
            if c.is_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '-'
            }
        })
        .collect()
}

/// Write `log_text` to a throwaway temp directory, then return a trimmed
/// head/tail excerpt of it and remove the directory (RAL-375) -- CI logs can
/// be arbitrarily large, so a scratch file (auto-cleaned here) rather than
/// `daemon/src/terminal_log.rs`'s durable retained store is the right model:
/// nothing from this ever needs to persist past producing the excerpt.
fn capture_and_trim_log(job_ref: &str, log_text: &str) -> String {
    let dir = std::env::temp_dir().join(format!(
        "ralphus-ci-watch-{}-{}",
        sanitize_path_segment(job_ref),
        now_ms()
    ));
    let captured = if std::fs::create_dir_all(&dir).is_ok() {
        let path = dir.join("failure.log");
        let content = std::fs::write(&path, log_text)
            .ok()
            .and_then(|()| std::fs::read_to_string(&path).ok());
        let _ = std::fs::remove_dir_all(&dir);
        content
    } else {
        None
    };
    trim_log_excerpt(
        captured.as_deref().unwrap_or(log_text),
        LOG_EXCERPT_LINES,
        LOG_EXCERPT_LINES,
    )
}

/// `(guardian_id, branch_id)` pairs with a watch currently in flight, so a
/// second feedback push on the same branch before the first watch finishes
/// doesn't spawn overlapping pollers for the same PR.
static WATCHING: LazyLock<Mutex<HashSet<(String, String)>>> =
    LazyLock::new(|| Mutex::new(HashSet::new()));

/// Record a CI-watch event in both the plain-text log and Cartographer, so
/// the poll loop's progress is queryable per-branch instead of only tailable
/// (RAL-98 pairing).
fn log_ci_watch(
    store: &Arc<Mutex<Store>>,
    guardian_id: &str,
    branch_id: &str,
    level: LogLevel,
    message: impl AsRef<str>,
    payload: serde_json::Value,
) {
    crate::cartographer::Note::new("ci-watch")
        .level(level)
        .scope("branch")
        .guardian(guardian_id)
        .emit(
            &store.lock().expect("poisoned"),
            message,
            serde_json::json!({"branch_id": branch_id, "detail": payload}),
        );
}

/// Start watching `branch_id`'s open PR after a review-feedback push
/// (RAL-375). No-op if the branch has no open, forge-numbered PR, its forge
/// client can't be resolved, or a watch for this exact branch is already
/// running -- fail-safe by design, matching `pr::check_pr_merges`'s
/// "an unreachable forge changes nothing" precedent, since a watch that
/// can't be started should never block or fail the push that triggered it.
pub fn watch_after_feedback_push(store: &Arc<Mutex<Store>>, guardian_id: &str, branch_id: &str) {
    let key = (guardian_id.to_string(), branch_id.to_string());
    {
        let mut watching = WATCHING.lock().expect("poisoned");
        if !watching.insert(key.clone()) {
            log_ci_watch(
                store,
                guardian_id,
                branch_id,
                LogLevel::DEBUG,
                format!(
                    "ralphus [ci-watch] review {guardian_id} branch {branch_id} watch skipped: already in flight"
                ),
                serde_json::json!({"outcome": "skipped", "reason": "already_in_flight"}),
            );
            return;
        }
    }
    let store = Arc::clone(store);
    std::thread::spawn(move || {
        run_watch(&store, &key.0, &key.1);
        WATCHING.lock().expect("poisoned").remove(&key);
    });
}

fn run_watch(store: &Arc<Mutex<Store>>, guardian_id: &str, branch_id: &str) {
    let Ok(guardian) = store.lock().expect("poisoned").get_guardian(guardian_id) else {
        return;
    };
    let Some(branch) = guardian.branches.iter().find(|b| b.id == branch_id) else {
        return;
    };
    let Ok(prs) = store
        .lock()
        .expect("poisoned")
        .list_pull_requests_for_guardian(guardian_id)
    else {
        return;
    };
    let Some(pr) = prs
        .iter()
        .find(|pr| pr.branch_id.as_deref() == Some(branch_id) && pr.state == "open")
    else {
        return;
    };
    let Some(number) = pr.pr_number else {
        return;
    };

    let root = PathBuf::from(&guardian.git_root);
    let forge_cfg = crate::config::resolve_forge(&root);
    let client = match crate::forge::resolve_remote(&root, &guardian.base_branch, &forge_cfg) {
        Ok(c) => c,
        Err(e) => {
            log_ci_watch(
                store,
                guardian_id,
                branch_id,
                LogLevel::WARNING,
                format!(
                    "ralphus [ci-watch] review {guardian_id} branch {branch_id} could not resolve forge client: {e}"
                ),
                serde_json::json!({"outcome": "unavailable", "error": e}),
            );
            return;
        }
    };

    log_ci_watch(
        store,
        guardian_id,
        branch_id,
        LogLevel::INFO,
        format!("ralphus [ci-watch] review {guardian_id} branch {branch_id} watching pr #{number}"),
        serde_json::json!({"pr_number": number, "outcome": "watching"}),
    );
    let start = Instant::now();
    loop {
        let elapsed = start.elapsed();
        if elapsed > MAX_WATCH_DURATION {
            log_ci_watch(
                store,
                guardian_id,
                branch_id,
                LogLevel::WARNING,
                format!(
                    "ralphus [ci-watch] review {guardian_id} branch {branch_id} pr #{number} \
                     gave up after {MAX_WATCH_DURATION:?} with no terminal status"
                ),
                serde_json::json!({"pr_number": number, "outcome": "timed_out"}),
            );
            return;
        }
        match client.check_pr_ci_status(number) {
            Ok(PrCiState::Passing) => {
                if elapsed < SUCCESS_SETTLE_DURATION {
                    log_ci_watch(
                        store,
                        guardian_id,
                        branch_id,
                        LogLevel::DEBUG,
                        format!(
                            "ralphus [ci-watch] review {guardian_id} branch {branch_id} pr #{number} \
                             appears passing but is still in the post-push settle window"
                        ),
                        serde_json::json!({"pr_number": number, "outcome": "settling"}),
                    );
                    std::thread::sleep(next_poll_delay(elapsed));
                    continue;
                }
                log_ci_watch(
                    store,
                    guardian_id,
                    branch_id,
                    LogLevel::INFO,
                    format!(
                        "ralphus [ci-watch] review {guardian_id} branch {branch_id} pr #{number} passing"
                    ),
                    serde_json::json!({"pr_number": number, "outcome": "passing"}),
                );
                return;
            }
            Ok(PrCiState::Failing(failure)) => {
                log_ci_watch(
                    store,
                    guardian_id,
                    branch_id,
                    LogLevel::WARNING,
                    format!(
                        "ralphus [ci-watch] review {guardian_id} branch {branch_id} pr #{number} \
                         failing: {}",
                        failure.reason
                    ),
                    serde_json::json!({"pr_number": number, "outcome": "failing", "reason": failure.reason}),
                );
                enqueue_ci_failure_notice(store, &guardian, branch, pr, &failure);
                return;
            }
            Ok(PrCiState::Pending) => {}
            Err(e) => {
                log_ci_watch(
                    store,
                    guardian_id,
                    branch_id,
                    LogLevel::DEBUG,
                    format!(
                        "ralphus [ci-watch] review {guardian_id} branch {branch_id} pr #{number} \
                         poll error (will retry): {e}"
                    ),
                    serde_json::json!({"pr_number": number, "outcome": "poll_error", "error": e}),
                );
            }
        }
        std::thread::sleep(next_poll_delay(elapsed));
    }
}

/// Enqueue the `"review"`-category mailbox notice for a terminal CI/merge
/// failure (RAL-375): PR URL, failing job URL (when the forge gave one), the
/// impacted review worktree, a trimmed log excerpt (when the forge gave
/// text), and a closing question asking whether to fix it in a subagent.
/// Wiring up that reply is out of scope here -- this only asks; it never
/// starts a subagent itself.
fn enqueue_ci_failure_notice(
    store: &Arc<Mutex<Store>>,
    guardian: &GuardianView,
    branch: &BranchView,
    pr: &PullRequestView,
    failure: &PrFailure,
) {
    let pr_url = pr
        .pr_url
        .clone()
        .unwrap_or_else(|| format!("{} PR/MR #{}", pr.forge, pr.pr_number.unwrap_or_default()));
    let worktree = branch
        .worktree
        .clone()
        .unwrap_or_else(|| "(worktree path unknown)".to_string());

    let mut text = format!(
        "CI/merge check failed for review '{}' ({}), branch '{}': {}\n\nPR: {pr_url}\n",
        guardian.name, guardian.id, branch.branch, failure.reason
    );
    if let Some(job_url) = &failure.job_url {
        text.push_str(&format!("Failing job: {job_url}\n"));
    }
    text.push_str(&format!("Impacted review worktree: {worktree}\n"));
    if let Some(log_text) = &failure.log_text {
        let excerpt = capture_and_trim_log(&pr.id, log_text);
        text.push_str(&format!("\nLog excerpt:\n{excerpt}\n"));
    }
    text.push_str("\nDo you want to fix these immediately in a subagent?");

    let entity_uri = format!("guardian:{}", guardian.id);
    let enqueued = {
        let guard = store.lock().expect("poisoned");
        guard.enqueue_mailbox_message_ex(
            MailboxPriority::High,
            &text,
            None,
            None,
            None,
            Some(&entity_uri),
            Some("review"),
        )
    };
    if let Err(e) = enqueued {
        log_ci_watch(
            store,
            &guardian.id,
            &branch.id,
            LogLevel::ERROR,
            format!(
                "ralphus [ci-watch] review {} branch {} could not enqueue ci-failure mailbox notice: {e}",
                guardian.id, branch.id
            ),
            serde_json::json!({"outcome": "mailbox_enqueue_failed", "error": e.to_string()}),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn poll_delay_starts_fast_then_backs_off_in_stages() {
        assert_eq!(next_poll_delay(Duration::ZERO), Duration::from_secs(5));
        assert_eq!(
            next_poll_delay(Duration::from_secs(29)),
            Duration::from_secs(5)
        );
        assert_eq!(
            next_poll_delay(Duration::from_secs(31)),
            Duration::from_secs(20)
        );
        assert_eq!(
            next_poll_delay(Duration::from_secs(4 * 60)),
            Duration::from_secs(20)
        );
        assert_eq!(
            next_poll_delay(Duration::from_secs(6 * 60)),
            Duration::from_secs(60)
        );
        assert_eq!(
            next_poll_delay(Duration::from_secs(60 * 60)),
            Duration::from_secs(60)
        );
    }

    #[test]
    fn trim_log_excerpt_leaves_a_short_log_untouched() {
        let text = "line1\nline2\nline3";
        assert_eq!(trim_log_excerpt(text, 5, 5), text);
    }

    #[test]
    fn trim_log_excerpt_keeps_head_and_tail_of_a_long_log() {
        let lines: Vec<String> = (1..=100).map(|n| format!("line{n}")).collect();
        let text = lines.join("\n");
        let trimmed = trim_log_excerpt(&text, 3, 3);
        assert!(trimmed.starts_with("line1\nline2\nline3\n"), "{trimmed}");
        assert!(trimmed.ends_with("line98\nline99\nline100"), "{trimmed}");
        assert!(trimmed.contains("94 line(s) omitted"), "{trimmed}");
        assert!(
            !trimmed.contains("line50"),
            "middle of the log must not survive trimming: {trimmed}"
        );
    }

    #[test]
    fn trim_log_excerpt_handles_an_exact_boundary_without_omitting_anything() {
        let text = "a\nb\nc\nd";
        // Exactly head+tail lines -- nothing should be described as omitted.
        assert_eq!(trim_log_excerpt(text, 2, 2), text);
    }

    #[test]
    fn capture_and_trim_log_round_trips_through_a_cleaned_up_temp_dir() {
        let lines: Vec<String> = (1..=200).map(|n| format!("log line {n}")).collect();
        let log = lines.join("\n");
        let excerpt = capture_and_trim_log("pr-test-1", &log);
        assert!(excerpt.starts_with("log line 1\n"), "{excerpt}");
        assert!(excerpt.ends_with("log line 200"), "{excerpt}");
        assert!(excerpt.len() < log.len(), "must actually trim: {excerpt}");
        // No leftover temp directories for this job ref.
        let leftover = std::fs::read_dir(std::env::temp_dir())
            .into_iter()
            .flatten()
            .flatten()
            .any(|e| {
                e.file_name()
                    .to_string_lossy()
                    .starts_with("ralphus-ci-watch-pr-test-1-")
            });
        assert!(!leftover, "temp dir must be cleaned up");
    }

    #[test]
    fn watch_after_feedback_push_is_a_noop_without_an_open_pr() {
        let store = Arc::new(Mutex::new(Store::open_in_memory().unwrap()));
        let root = std::env::temp_dir();
        let guardian_id = {
            let guard = store.lock().unwrap();
            guard
                .create_guardian("r", "main", &root.to_string_lossy())
                .unwrap()
        };
        let branch_id = {
            let guard = store.lock().unwrap();
            guard
                .add_guardian_branch(&guardian_id, "feature/a")
                .unwrap();
            guard.get_guardian(&guardian_id).unwrap().branches[0]
                .id
                .clone()
        };
        // Must return promptly (no PR to watch) rather than spawning a poll
        // that lingers past this test. The removal happens on a spawned
        // thread, so poll for it instead of a fixed sleep -- a single sleep
        // is prone to false failures under heavy parallel test load, where
        // the OS scheduler can take well over 50ms to run the thread.
        watch_after_feedback_push(&store, &guardian_id, &branch_id);
        let key = (guardian_id, branch_id);
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline && WATCHING.lock().unwrap().contains(&key) {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(
            !WATCHING.lock().unwrap().contains(&key),
            "a no-op watch must not remain marked in flight"
        );
    }
}
