//! Watch a PR's CI/CD + mergeability after it's opened or after a
//! review-feedback push (RAL-375, extended by RAL-<new>), and (RAL-395) a
//! standing poll of every open PR that persists the result and can trigger
//! an auto-fix dispatch.
//!
//! [`start_ci_watch`] is called right after a branch's PR/MR is first
//! submitted (`crate::pr::submit_stacked_branch_pr`) and right after
//! `guardian_merge::run_feedback` pushes a new commit onto a stacked branch:
//! if that branch has an open forge PR (`crate::pr::PullRequestView`), it
//! spawns a background poll of [`crate::forge::ForgeClient::check_pr_ci_status`]
//! -- fast at first to catch a quick pipeline, backing off for a slow one
//! (see [`next_poll_delay`]) -- until a terminal state. A terminal failure
//! drops a `"review"`-category mailbox notice (`crate::mailbox`) naming the
//! PR, the failing job, the impacted review worktree, and a trimmed log
//! excerpt, then asks whether to fix it immediately in a subagent. A
//! terminal success is silent.
//!
//! Submitting a stack opens one PR at a time (a git push plus a forge API
//! call per branch, often tens of seconds apart) -- without a watch starting
//! the moment each PR exists, a branch submitted early in the same pass could
//! have its CI status known well before a sibling submitted moments later,
//! whose own first check only arrives once [`poll_open_pr_ci_status`]'s
//! coarser, per-guardian-throttled sweep gets around to it (RAL-<new>: this
//! is what produced two sibling PRs' board badges updating minutes apart even
//! though both were already green on the forge).
//!
//! [`poll_open_pr_ci_status`] (RAL-395) is the standing counterpart: called
//! on every `review_maintenance` pass (throttled per guardian, see
//! [`STANDING_POLL_INTERVAL`]) so a PR's CI status is known to the board even
//! when no submission or feedback push has recently fired [`start_ci_watch`]
//! -- e.g. a watch that gave up after [`MAX_WATCH_DURATION`], or a daemon
//! restart losing the in-memory [`WATCHING`] set. It persists every poll's outcome
//! (`crate::pr::PullRequestView::ci_status`) and, when the guardian has
//! opted into `auto_fix_pr_errors`, dispatches [`dispatch_pr_auto_fix`] on a
//! freshly observed failure. That dispatch posts its own feedback message
//! into the branch's feedback thread, attributed to [`AUTO_FIX_AUTHOR`]
//! rather than any human reviewer, so it reads as its own provenance in the
//! board's chat thread instead of blending in with a person's own feedback.
//!
//! There is no new tracking type here: a PR is already reachable from a
//! review worktree via the `guardian_id`/`branch_id` pair
//! `PullRequestView` and `BranchView` share.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock, Mutex};
use std::time::{Duration, Instant};

use crate::cancel::CancelToken;
use crate::forge::{FailedCheck, PrCiState, PrFailure};
use crate::guardian::{BranchView, GuardianView};
use crate::logging::LogLevel;
use crate::mailbox::MailboxPriority;
use crate::pr::PullRequestView;
use crate::runner::Runner;
#[cfg(test)]
use crate::store::Store;
use crate::store::now_ms;

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
    store: &crate::store_lock::StoreHandle,
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
            &store.lock(),
            message,
            serde_json::json!({"branch_id": branch_id, "detail": payload}),
        );
}

/// Start watching `branch_id`'s open PR (RAL-375, extended by RAL-<new> to
/// also fire right after a PR is first submitted, not only after a
/// review-feedback push). No-op if the branch has no open, forge-numbered
/// PR, its forge client can't be resolved, or a watch for this exact branch
/// is already running -- fail-safe by design, matching
/// `pr::check_pr_merges`'s "an unreachable forge changes nothing" precedent,
/// since a watch that can't be started should never block or fail whatever
/// triggered it.
pub fn start_ci_watch(store: &crate::store_lock::StoreHandle, guardian_id: &str, branch_id: &str) {
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

fn run_watch(store: &crate::store_lock::StoreHandle, guardian_id: &str, branch_id: &str) {
    let Ok(guardian) = store.lock().get_guardian(guardian_id) else {
        return;
    };
    let Some(branch) = guardian.branches.iter().find(|b| b.id == branch_id) else {
        return;
    };
    let Ok(prs) = store.lock().list_pull_requests_for_guardian(guardian_id) else {
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
    // RAL-338 follow-up: resolved per-PR via `pr.repo`, not a single client
    // derived from the guardian's own base branch -- a fork-routed stack's
    // root PR is filed against the parent while every other stacked PR is
    // filed against the fork, so a single guardian-wide client can only ever
    // answer for one of them (see `crate::pr::PrRepoRouting`'s doc comment).
    let client = match crate::pr::forge_client_for_pr(
        store,
        &root,
        &guardian.base_branch,
        &forge_cfg,
        &pr.repo,
        guardian.owner.as_deref(),
    ) {
        Some(c) => c,
        None => {
            log_ci_watch(
                store,
                guardian_id,
                branch_id,
                LogLevel::WARNING,
                format!(
                    "ralphus [ci-watch] review {guardian_id} branch {branch_id} could not resolve forge client"
                ),
                serde_json::json!({"outcome": "unavailable"}),
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
                let _ = store
                    .lock()
                    .set_pr_ci_status(&pr.id, PrCiState::Passing.as_str(), None);
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
                let _ = store.lock().set_pr_ci_status(
                    &pr.id,
                    PrCiState::Failing(failure.clone()).as_str(),
                    failure.job_url.as_deref(),
                );
                enqueue_ci_failure_notice(store, &guardian, branch, pr, &failure);
                return;
            }
            Ok(PrCiState::Pending) => {
                // RAL-462: persist this immediately rather than leaving
                // whatever terminal status (e.g. a prior "failing") was
                // recorded before this watch started -- without this, a
                // board badge stays stuck on that stale verdict for as long
                // as this watch keeps polling, potentially its entire
                // `MAX_WATCH_DURATION`, instead of reflecting that the
                // forge already considers the new commit's CI in flight.
                let _ = store
                    .lock()
                    .set_pr_ci_status(&pr.id, PrCiState::Pending.as_str(), None);
            }
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
/// failure (RAL-375): PR URL, every failing job/check's own URL (RAL-<new>;
/// a single "Failing job:" line when there's only one, a bulleted list when
/// there's more), the impacted review worktree, a trimmed log excerpt from
/// the first failing check (when the forge gave text), and a closing
/// question asking whether to fix it in a subagent. Wiring up that reply is
/// out of scope here -- this only asks; it never starts a subagent itself.
fn enqueue_ci_failure_notice(
    store: &crate::store_lock::StoreHandle,
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
    if failure.checks.len() > 1 {
        text.push_str(&format!("{} failing checks:\n", failure.checks.len()));
        for check in &failure.checks {
            let url = check.job_url.as_deref().unwrap_or("(no URL)");
            text.push_str(&format!("- {}: {url}\n", check.name));
        }
    } else if let Some(job_url) = &failure.job_url {
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
        let guard = store.lock();
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

/// Tell a human (mailbox/notification center) that this PR's single auto-fix
/// attempt is exhausted and CI is still failing (RAL-<new>): the background
/// poller will not try again on its own until either a new commit lands
/// (`Store::update_pull_request_ex`'s sha-advance clear) or CI is next
/// observed non-failing (`Store::set_pr_ci_status`'s clear) -- both of which
/// require *something* to change first, so without this notice a PR can sit
/// failing indefinitely with nothing telling a person it stopped trying.
/// Fires at most once per exhausted attempt (`auto_fix_exhausted_notified_at_ms`,
/// checked by the caller before calling this), mirroring the single-attempt
/// cap itself so a long-stuck PR doesn't re-notify on every 2-minute standing
/// poll.
fn enqueue_auto_fix_exhausted_notice(
    store: &crate::store_lock::StoreHandle,
    guardian: &GuardianView,
    pr: &PullRequestView,
    failure: &PrFailure,
) {
    let pr_url = pr
        .pr_url
        .clone()
        .unwrap_or_else(|| format!("{} PR/MR #{}", pr.forge, pr.pr_number.unwrap_or_default()));

    let mut text = format!(
        "Auto-fix exhausted for review '{}' ({}), PR: {pr_url}\n\n\
         The one automatic auto-fix attempt for this failure has already run and CI is still \
         failing: {}\n\nThe background poller will not try again on its own until a new commit \
         is pushed to this PR.\n",
        guardian.name, guardian.id, failure.reason
    );
    if failure.checks.len() > 1 {
        text.push_str(&format!("{} failing checks:\n", failure.checks.len()));
        for check in &failure.checks {
            let url = check.job_url.as_deref().unwrap_or("(no URL)");
            text.push_str(&format!("- {}: {url}\n", check.name));
        }
    } else if let Some(job_url) = &failure.job_url {
        text.push_str(&format!("Failing job: {job_url}\n"));
    }
    text.push_str(
        "\nTo force another attempt: push a new commit, or use \"Action Feedback\" on this PR \
         (bypasses the single-attempt cap for a person-initiated retry).",
    );

    let entity_uri = format!("guardian:{}", guardian.id);
    let enqueued = {
        let guard = store.lock();
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
    match enqueued {
        Ok(_) => {
            log_ci_watch(
                store,
                &guardian.id,
                pr.branch_id.as_deref().unwrap_or(""),
                LogLevel::WARNING,
                format!(
                    "ralphus [ci-watch] review {} pr #{} auto-fix exhausted: notified mailbox",
                    guardian.id,
                    pr.pr_number.unwrap_or_default()
                ),
                serde_json::json!({"pr_number": pr.pr_number, "outcome": "exhausted_notified"}),
            );
            if let Err(e) = store.lock().mark_pr_auto_fix_exhausted_notified(&pr.id) {
                log_ci_watch(
                    store,
                    &guardian.id,
                    pr.branch_id.as_deref().unwrap_or(""),
                    LogLevel::ERROR,
                    format!(
                        "ralphus [ci-watch] review {} pr #{} could not record auto-fix-exhausted \
                         notice as sent: {e}",
                        guardian.id,
                        pr.pr_number.unwrap_or_default()
                    ),
                    serde_json::json!({"outcome": "exhausted_notice_mark_failed", "error": e.to_string()}),
                );
            }
        }
        Err(e) => {
            log_ci_watch(
                store,
                &guardian.id,
                pr.branch_id.as_deref().unwrap_or(""),
                LogLevel::ERROR,
                format!(
                    "ralphus [ci-watch] review {} pr #{} could not enqueue auto-fix-exhausted \
                     mailbox notice: {e}",
                    guardian.id,
                    pr.pr_number.unwrap_or_default()
                ),
                serde_json::json!({"outcome": "exhausted_notice_enqueue_failed", "error": e.to_string()}),
            );
        }
    }
}

// ---------------------------------------------------------------------------
// RAL-395: standing poll of every open PR + auto-fix dispatch
// ---------------------------------------------------------------------------

/// Minimum interval between standing CI-status polls for the same guardian
/// (RAL-395) -- independent of [`start_ci_watch`]'s fast-then-
/// backoff poll of a single just-opened or just-pushed branch. [`poll_open_pr_ci_status`]
/// is cheap to call on every `review_maintenance` pass (a 5s cadence), so it
/// needs its own, much coarser throttle to stay within forge rate-limit
/// expectations (`.agent/forge-design-principles.md`).
const STANDING_POLL_INTERVAL: Duration = Duration::from_secs(2 * 60);

/// Last standing-poll time per guardian id (RAL-395) -- mirrors
/// `guardian_merge::IDLE_MAINT_LAST`'s shape, but keyed and intervaled
/// independently since this throttles a forge call, not a maintenance pass.
static STANDING_POLL_LAST: LazyLock<Mutex<HashMap<String, Instant>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// Poll every open, forge-numbered PR of `guardian_id` for its current CI
/// status (RAL-395), persist the result (`crate::pr::PullRequestView::ci_status`)
/// so the board can read it without a live forge call on every page load, and
/// dispatch [`dispatch_pr_auto_fix`] on a freshly observed failure when the
/// guardian has opted into `auto_fix_pr_errors`. Throttled to at most once
/// per [`STANDING_POLL_INTERVAL`] per guardian -- safe to call on every
/// `review_maintenance` pass. Fail-safe by design, matching
/// [`start_ci_watch`]'s precedent: an unresolvable forge client or a
/// poll error for one PR never blocks or fails the caller, and never stops
/// the remaining PRs in the same guardian from being polled.
pub fn poll_open_pr_ci_status(
    store: &crate::store_lock::StoreHandle,
    runner: &dyn Runner,
    guardian_id: &str,
) {
    {
        let mut last = STANDING_POLL_LAST.lock().expect("poisoned");
        let now = Instant::now();
        if last
            .get(guardian_id)
            .is_some_and(|prev| now.duration_since(*prev) < STANDING_POLL_INTERVAL)
        {
            return;
        }
        last.insert(guardian_id.to_string(), now);
    }
    let Ok(guardian) = store.lock().get_guardian(guardian_id) else {
        return;
    };
    if crate::guardian::GuardianStatus::is_terminal_status(&guardian.status) {
        return;
    }
    let Ok(prs) = store.lock().list_pull_requests_for_guardian(guardian_id) else {
        return;
    };
    let open: Vec<PullRequestView> = prs
        .into_iter()
        .filter(|p| p.state == "open" && p.pr_number.is_some())
        .collect();
    if open.is_empty() {
        return;
    }
    let root = PathBuf::from(&guardian.git_root);
    let forge_cfg = crate::config::resolve_forge(&root);
    // Known slow spot: every open PR in this guardian is polled sequentially
    // here (2-3 blocking GitHub calls each), all inside the same
    // `STANDING_POLL_INTERVAL` window. Fine for the PR counts seen so far
    // (a review's open-PR list, not the whole world) -- a review with a
    // handful to ~a dozen PRs finishes in a few seconds either way, and
    // nothing user-facing blocks on this pass. Revisit only if a review's
    // open-PR count grows large enough that this loop's wall-clock time
    // becomes a meaningful fraction of `STANDING_POLL_INTERVAL` itself,
    // making the tail PRs' statuses noticeably stale by the time this pass
    // reaches them. If that happens, prefer a small bounded worker pool
    // (a few `std::thread::spawn`s pulling from this same `open` list) over
    // pulling in an async runtime (Tokio) -- the daemon has no async
    // boundary anywhere else, every dependency here (`ureq`, `rusqlite`,
    // `git2`) is sync, and GitHub's API itself penalizes unbounded
    // concurrent requests via its secondary rate limit, so bounded threads
    // solve this more directly than an executor would.
    let mut polled: Vec<(PullRequestView, PrCiState)> = Vec::with_capacity(open.len());
    for pr in open {
        let number = pr.pr_number.expect("filtered above");
        // RAL-338 follow-up: resolved per-PR via `pr.repo` -- a fork-routed
        // stack's root PR is filed against the parent while every other
        // stacked PR is filed against the fork, so a single guardian-wide
        // client (as this used to resolve once before the loop) can only
        // ever answer for one of them, and silently 404s polling the rest.
        let Some(client) = crate::pr::forge_client_for_pr(
            store,
            &root,
            &guardian.base_branch,
            &forge_cfg,
            &pr.repo,
            guardian.owner.as_deref(),
        ) else {
            log_ci_watch(
                store,
                guardian_id,
                pr.branch_id.as_deref().unwrap_or(""),
                LogLevel::DEBUG,
                format!(
                    "ralphus [ci-watch] review {guardian_id} pr #{number} standing poll: could not resolve forge client"
                ),
                serde_json::json!({"pr_number": number, "outcome": "unavailable"}),
            );
            continue;
        };
        let probe = match client.check_pr_ci_status_probe(number) {
            Ok(p) => p,
            Err(e) => {
                log_ci_watch(
                    store,
                    guardian_id,
                    pr.branch_id.as_deref().unwrap_or(""),
                    LogLevel::DEBUG,
                    format!(
                        "ralphus [ci-watch] review {guardian_id} pr #{number} standing poll error (will retry next pass): {e}"
                    ),
                    serde_json::json!({"pr_number": number, "outcome": "poll_error", "error": e}),
                );
                continue;
            }
        };
        let state = probe.ci;
        let job_url = match &state {
            PrCiState::Failing(f) => f.job_url.clone(),
            _ => None,
        };
        let _ = store
            .lock()
            .set_pr_ci_status(&pr.id, state.as_str(), job_url.as_deref());
        let _ = store.lock().set_pr_draft(&pr.id, probe.draft);
        polled.push((pr, state));
    }
    // RAL-<new>: dispatch (or defer) only once every open PR's state for this
    // pass is known -- see `plan_auto_fix_dispatch` for why stack order
    // matters here.
    for decision in plan_auto_fix_dispatch(&guardian, &polled) {
        if decision.dispatch {
            let Some(client) = crate::pr::forge_client_for_pr(
                store,
                &root,
                &guardian.base_branch,
                &forge_cfg,
                &decision.pr.repo,
                guardian.owner.as_deref(),
            ) else {
                continue;
            };
            dispatch_pr_auto_fix(
                store,
                runner,
                &guardian,
                decision.pr,
                decision.failure,
                &client,
            );
        } else {
            log_ci_watch(
                store,
                guardian_id,
                decision.pr.branch_id.as_deref().unwrap_or(""),
                LogLevel::DEBUG,
                format!(
                    "ralphus [ci-watch] review {guardian_id} pr #{} auto-fix deferred: an \
                     earlier branch in this review's stack still has a failing PR -- fixing \
                     this branch now would just be redone once the upstream fix lands",
                    decision.pr.pr_number.unwrap_or_default()
                ),
                serde_json::json!({
                    "pr_number": decision.pr.pr_number,
                    "outcome": "deferred_upstream_failing",
                }),
            );
        }
    }
}

/// One failing PR's auto-fix eligibility for this poll pass, from
/// [`plan_auto_fix_dispatch`].
struct AutoFixDecision<'a> {
    pr: &'a PullRequestView,
    failure: &'a PrFailure,
    /// `true` when no earlier-position branch in the same stack also has a
    /// currently-failing PR.
    dispatch: bool,
}

/// This guardian's `[BranchView::position]` for the branch a PR was opened
/// against (RAL-<new>): `i64::MAX` for a PR with no resolvable branch (the
/// combined-worktree PR, `branch_id: None`, or a `branch_id` that no longer
/// matches an enabled branch) -- mirroring `dispatch_pr_auto_fix`'s own
/// fallback for the same case, so an unresolvable PR sorts after every real
/// branch position: blockable by any branch's failure, never itself blocking
/// one.
fn pr_stack_position(guardian: &GuardianView, pr: &PullRequestView) -> i64 {
    pr.branch_id
        .as_deref()
        .and_then(|bid| guardian.branches.iter().find(|b| b.id == bid))
        .filter(|b| b.enabled)
        .map_or(i64::MAX, |b| b.position)
}

/// Decide which of this pass's failing PRs are safe to auto-fix right now
/// (RAL-<new>): a stacked review branch is rebuilt on top of every
/// earlier-position branch's content on its next restack, so auto-fixing a
/// downstream branch while an earlier-position branch's PR is STILL failing
/// just repeats the same root-cause fix twice -- once on code that's about
/// to be rebased out from under it the moment the upstream fix lands -- or
/// worse, teaches the downstream branch to route around a bug that actually
/// belongs upstream. Blocks strictly on stack position, not on whether the
/// upstream PR's own auto-fix attempt has already been used up (RAL-395's
/// single-attempt-per-failure budget): an exhausted, still-failing upstream
/// attempt leaves the same broken code in place, so downstream must keep
/// waiting for a human either way.
fn plan_auto_fix_dispatch<'a>(
    guardian: &GuardianView,
    polled: &'a [(PullRequestView, PrCiState)],
) -> Vec<AutoFixDecision<'a>> {
    let mut ordered: Vec<(i64, &PullRequestView, &PrFailure)> = polled
        .iter()
        .filter_map(|(pr, state)| match state {
            PrCiState::Failing(failure) => Some((pr_stack_position(guardian, pr), pr, failure)),
            _ => None,
        })
        .collect();
    ordered.sort_by_key(|(position, ..)| *position);
    let mut upstream_failing = false;
    ordered
        .into_iter()
        .map(|(_, pr, failure)| {
            let decision = AutoFixDecision {
                pr,
                failure,
                dispatch: !upstream_failing,
            };
            upstream_failing = true;
            decision
        })
        .collect()
}

/// How many lines of a CI failure log trigger the "this may be very large"
/// caution in the auto-fix prompt (RAL-395, interview Q6) -- an arbitrary but
/// generous threshold; the point is giving the agent a size signal, not
/// precisely classifying "large".
const LARGE_LOG_LINE_THRESHOLD: usize = 500;

/// The on-disk path a failing check/job's log gets written to (RAL-395,
/// extended for RAL-<new>'s per-check breakdown): when there's exactly one
/// failing check, the original single, stably-named `.ralphus-ci-failure.log`
/// -- unchanged from before per-check breakdown existed. With more than one,
/// each gets its own `.ralphus-ci-failure-<name>.log` so no failing check's
/// log clobbers another's.
fn ci_failure_log_path(worktree: &str, check_name: &str, only_one: bool) -> PathBuf {
    if only_one {
        PathBuf::from(worktree).join(".ralphus-ci-failure.log")
    } else {
        PathBuf::from(worktree).join(format!(
            ".ralphus-ci-failure-{}.log",
            sanitize_path_segment(check_name)
        ))
    }
}

/// Write one failing check/job's full, untrimmed CI failure log to `path`
/// inside the branch's own worktree (RAL-395, interview Q6) -- colocated with
/// wherever the auto-fix agent actually runs (the same `cwd` `run_feedback`
/// gives it, local or remote), unlike `capture_and_trim_log`'s throwaway
/// daemon-host temp directory, which only ever produces a short excerpt for a
/// mailbox message a human reads on this machine. Overwrites any previous
/// failure log at the same path -- only the most recent failure is ever
/// relevant. Best-effort: returns `None` (logged) if the write fails, and the
/// caller still dispatches auto-fix without an on-disk path in that case.
fn write_ci_failure_log(path: &Path, log_text: &str) -> Option<PathBuf> {
    std::fs::write(path, log_text)
        .ok()
        .map(|()| path.to_path_buf())
}

/// Render one failing check/job as a self-contained paragraph for the
/// auto-fix prompt (RAL-<new>): its reason, its own job URL (when the forge
/// gave one), and where its own log ended up (or a note that none was
/// available) -- everything [`dispatch_pr_auto_fix`] joins one of these per
/// failing check into the final prompt, so an agent facing several failures
/// at once gets every one of their URLs up front instead of just the first.
/// `reason_line` is passed in rather than derived from `check.name` here
/// because the caller already knows whether this is the failure's only check
/// (in which case `PrFailure::reason` already reads naturally, e.g. "check
/// 'build' failed") or one of several (in which case a generic "'name'
/// failed" is used, since the specific noun -- check/job/status -- isn't
/// carried per-check).
fn describe_failing_check(
    worktree: Option<&str>,
    check: &FailedCheck,
    reason_line: &str,
    only_one: bool,
) -> String {
    // RAL-<new>: a job's name alone can be misleadingly broad -- see
    // `FailedCheck::failing_step`'s doc comment for the incident this
    // prevents. When known, name the actual failing step explicitly and
    // warn against assuming the job's other bundled step(s) are the problem
    // just because they share its name.
    let step_note = check
        .failing_step
        .as_deref()
        .map_or_else(String::new, |step| {
            format!(
                "This job/check may bundle more than one distinct verification; the one that \
             actually failed is: '{step}'. Verify and fix THAT specifically -- do not assume a \
             different, similarly-named step/check under the same job is the problem.\n"
            )
        });
    let job_note = check
        .job_url
        .as_deref()
        .map_or_else(String::new, |url| format!("Failing job: {url}\n"));
    let log_note = match (worktree, &check.log_text) {
        (Some(worktree), Some(log_text)) => {
            let line_count = log_text.lines().count();
            let path = ci_failure_log_path(worktree, &check.name, only_one);
            match write_ci_failure_log(&path, log_text) {
                Some(path) => {
                    let size_note = if line_count > LARGE_LOG_LINE_THRESHOLD {
                        format!(
                            " This log has {line_count} lines and may be very large (tens of \
                             thousands of lines in the worst case) -- use your judgment about \
                             whether to read it in full or just the parts that look relevant \
                             (e.g. the tail, or a search for the failing check's name)."
                        )
                    } else {
                        String::new()
                    };
                    format!(
                        "The full CI failure log ({line_count} line(s)) was written to \
                         {} on this machine.{size_note}",
                        path.display()
                    )
                }
                None => "The CI failure log could not be written to disk; \
                         only the failure summary below is available."
                    .to_string(),
            }
        }
        (_, Some(_)) => {
            "A CI failure log is available from the forge, but this branch has no known \
             worktree path to write it into."
                .to_string()
        }
        (_, None) => "The forge did not provide a CI failure log for this job.".to_string(),
    };
    format!("Reason: {reason_line}\n{step_note}{job_note}{log_note}")
}

/// Attributed `author` (RAL-379 semantics) for the feedback message
/// [`dispatch_pr_auto_fix`] posts to a branch's feedback thread -- the same
/// field a human reviewer's name renders from, so the board shows this round
/// came from the auto-fix system rather than a person.
pub const AUTO_FIX_AUTHOR: &str = "PR Auto-Fix";

/// Audit-only `submitted_by` value (RAL-379 semantics: never rendered) paired
/// with [`AUTO_FIX_AUTHOR`], identifying the subsystem that posted the message.
const AUTO_FIX_SUBMITTED_BY: &str = "guardian:ci-watch-auto-fix";

/// Dispatch the review's agent to fix a failing PR/MR (RAL-395): composes the
/// auto-fix prompt (project/per-review template, `<<prompt>>` replaced by the
/// failing branch's own Cells' prompts, `{insert URL here}` replaced by the
/// PR's URL when present), posts it to the branch's feedback thread
/// (`guardian::MessageView`) attributed to [`AUTO_FIX_AUTHOR`] so it's visibly
/// distinct from human-authored feedback, and runs it through
/// `guardian_merge::run_feedback` with `require_proof: true`, gating
/// success/failure specifically on the agent's own `RALPHUS_PROOF` verdict
/// (interview Q7) rather than `run_feedback`'s own `Done`/`Failed`
/// distinction, which doesn't tell "agent fixed it" apart from "agent gave up
/// without erroring".
///
/// No-ops when `auto_fix_pr_errors` isn't enabled for this guardian, or when
/// auto-fix was already attempted for this PR's *current* failure (single
/// attempt per failure, interview Q5) -- the attempted marker is set before
/// the (potentially long-running) agent call, not after, so a second
/// standing-poll tick landing mid-dispatch can never double-fire it.
///
/// `pub` (rather than `pub(crate)`) specifically so `daemon/tests/
/// guardian_merge.rs`'s existing stacked-branch fixtures can drive this
/// directly, the same way that file already calls `run_feedback` -- the
/// PR-stacking regression coverage this needs (an auto-fix commit must still
/// fold into the review's linear stack and restack correctly) belongs
/// alongside `run_feedback`'s other restack tests, not duplicated here.
/// Attributed `author` for [`dispatch_pr_fix_manual`] when no requester
/// identity resolved at the HTTP boundary -- distinct from [`AUTO_FIX_AUTHOR`]
/// so a manual, person-initiated fix never renders under the automated
/// system's own name.
pub const MANUAL_PR_FIX_AUTHOR: &str = "Manual (PR fix)";

/// Resolve which branch a PR's fix (auto or manual) targets: the PR's own
/// recorded branch, or (for a whole-stack PR) the topmost enabled stacked
/// branch, since the combined worktree itself is read-only.
fn pr_fix_branch_id(guardian: &GuardianView, pr: &PullRequestView) -> Option<String> {
    match &pr.branch_id {
        Some(bid) => Some(bid.clone()),
        None => guardian
            .branches
            .iter()
            .filter(|b| b.enabled)
            .max_by_key(|b| b.position)
            .map(|b| b.id.clone()),
    }
}

pub fn dispatch_pr_auto_fix(
    store: &crate::store_lock::StoreHandle,
    runner: &dyn Runner,
    guardian: &GuardianView,
    pr: &PullRequestView,
    failure: &PrFailure,
    client: &crate::forge::ForgeClient,
) {
    if !guardian.auto_fix_pr_errors.unwrap_or(false) {
        log_ci_watch(
            store,
            &guardian.id,
            pr.branch_id.as_deref().unwrap_or(""),
            LogLevel::DEBUG,
            format!(
                "ralphus [ci-watch] review {} pr #{} auto-fix skipped: auto_fix_pr_errors is not \
                 enabled for this review",
                guardian.id,
                pr.pr_number.unwrap_or_default()
            ),
            serde_json::json!({"pr_number": pr.pr_number, "outcome": "skipped_not_enabled"}),
        );
        return;
    }
    if pr.auto_fix_attempted_at_ms.is_some() {
        // RAL-<new>: this is the single-attempt-per-failure cap from this
        // function's own doc comment -- previously silent, which is exactly
        // what made guardian-000000000119 / PR #235 look "stuck" instead of
        // deliberately capped. Logged at the same level/shape as the
        // stack-order deferral below so both no-retry reasons show up the
        // same way on a PR's timeline.
        log_ci_watch(
            store,
            &guardian.id,
            pr.branch_id.as_deref().unwrap_or(""),
            LogLevel::DEBUG,
            format!(
                "ralphus [ci-watch] review {} pr #{} auto-fix skipped: already attempted for this \
                 PR's current failure (single attempt per failure)",
                guardian.id,
                pr.pr_number.unwrap_or_default()
            ),
            serde_json::json!({
                "pr_number": pr.pr_number,
                "outcome": "skipped_already_attempted",
                "auto_fix_attempted_at_ms": pr.auto_fix_attempted_at_ms,
            }),
        );
        // RAL-<new>: a debug log line in Cartographer is easy to miss --
        // the human this actually matters to needs it in the notification
        // center, once per exhausted attempt (not every 2-minute poll while
        // it stays exhausted).
        if pr.auto_fix_exhausted_notified_at_ms.is_none() {
            enqueue_auto_fix_exhausted_notice(store, guardian, pr, failure);
        }
        return;
    }
    let Some(branch_id) = pr_fix_branch_id(guardian, pr) else {
        log_ci_watch(
            store,
            &guardian.id,
            pr.branch_id.as_deref().unwrap_or(""),
            LogLevel::WARNING,
            format!(
                "ralphus [ci-watch] review {} pr #{} auto-fix skipped: could not resolve a branch \
                 to apply the fix to",
                guardian.id,
                pr.pr_number.unwrap_or_default()
            ),
            serde_json::json!({"pr_number": pr.pr_number, "outcome": "skipped_no_branch"}),
        );
        return;
    };

    // RAL-395: single attempt per failure -- claim it now, before the
    // (potentially long-running) agent dispatch below, not after.
    let _ = store.lock().mark_pr_auto_fix_attempted(&pr.id);

    run_pr_fix(
        store,
        runner,
        guardian,
        pr,
        &branch_id,
        failure,
        client,
        AUTO_FIX_AUTHOR,
        Some(AUTO_FIX_SUBMITTED_BY),
    );
}

/// Manual counterpart to [`dispatch_pr_auto_fix`] (RAL-<new>): dispatches the
/// same CI-failure fix, but as an explicit person-initiated override rather
/// than the unattended background poller's own single attempt. Deliberately
/// bypasses both gates `dispatch_pr_auto_fix` enforces:
/// - `guardian.auto_fix_pr_errors` -- that toggle controls whether the
///   *background poller* may act unattended; it says nothing about whether a
///   person is allowed to ask for a fix directly, which is a distinct
///   authorization already granted by them clicking the button.
/// - `pr.auto_fix_attempted_at_ms` -- the single-attempt-per-failure cap only
///   exists to stop the unattended poller from hammering a stuck failure; a
///   person retrying by hand is exactly the case that cap should not block.
///
/// Still claims `auto_fix_attempted_at_ms` immediately before dispatching
/// (same ordering as `dispatch_pr_auto_fix`), so a standing-poll tick landing
/// concurrently sees an attempt already in flight and does not also fire.
///
/// `submitted_by` is the registered user who triggered this (resolved at the
/// HTTP boundary); the message is attributed to them by name, or
/// [`MANUAL_PR_FIX_AUTHOR`] if no identity resolved, so the board's chat
/// thread never shows this as coming from the automated system.
#[allow(clippy::too_many_arguments)]
pub fn dispatch_pr_fix_manual(
    store: &crate::store_lock::StoreHandle,
    runner: &dyn Runner,
    guardian: &GuardianView,
    pr: &PullRequestView,
    branch_id: &str,
    failure: &PrFailure,
    client: &crate::forge::ForgeClient,
    submitted_by: Option<&str>,
) {
    let _ = store.lock().mark_pr_auto_fix_attempted(&pr.id);
    let author = submitted_by
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or(MANUAL_PR_FIX_AUTHOR);
    run_pr_fix(
        store,
        runner,
        guardian,
        pr,
        branch_id,
        failure,
        client,
        author,
        submitted_by,
    );
}

/// Shared core of [`dispatch_pr_auto_fix`] and [`dispatch_pr_fix_manual`]:
/// build the auto-fix-style prompt for `failure`, post it into `branch_id`'s
/// feedback thread attributed to `author`/`submitted_by`, and run it through
/// `guardian_merge::run_feedback` with `require_proof: true`. Does not gate
/// on `auto_fix_pr_errors` or claim/consult `auto_fix_attempted_at_ms` --
/// callers own their own gating and claiming before calling this.
#[allow(clippy::too_many_arguments)]
fn run_pr_fix(
    store: &crate::store_lock::StoreHandle,
    runner: &dyn Runner,
    guardian: &GuardianView,
    pr: &PullRequestView,
    branch_id: &str,
    failure: &PrFailure,
    client: &crate::forge::ForgeClient,
    author: &str,
    submitted_by: Option<&str>,
) {
    let Some(branch) = guardian.branches.iter().find(|b| b.id == branch_id) else {
        return;
    };

    let cell_prompts = store
        .lock()
        .cell_prompts_for_review_branch(&guardian.id, &branch.branch)
        .unwrap_or_default();

    // RAL-<new>: one self-contained paragraph per failing check/job -- when a
    // forge poll found more than one (`failure.checks`), the agent gets every
    // one of their URLs/logs up front instead of just the first, since this
    // poll already queried them all deterministically and re-deriving that
    // list itself would just repeat work already done. When there's no
    // per-check breakdown at all (a merge-conflict verdict, or a pipeline
    // that failed before any job could be attributed), fall back to a single
    // paragraph built from the summary fields, same as before per-check
    // breakdown existed.
    let only_one = failure.checks.len() <= 1;
    let paragraphs: Vec<String> = if failure.checks.is_empty() {
        vec![describe_failing_check(
            branch.worktree.as_deref(),
            &FailedCheck {
                name: String::new(),
                job_url: failure.job_url.clone(),
                log_text: failure.log_text.clone(),
                failing_step: None,
            },
            &failure.reason,
            only_one,
        )]
    } else {
        failure
            .checks
            .iter()
            .map(|check| {
                let reason_line = if only_one {
                    failure.reason.clone()
                } else {
                    format!("'{}' failed", check.name)
                };
                // RAL-<new>: a single named job/check can bundle several
                // differently-purposed steps (the incident that motivated
                // this: GitHub's "Docs (screenshot coverage lint)" job also
                // runs an unrelated cli-reference.md freshness check as a
                // separate step) -- an agent told only the job name can end
                // up verifying the wrong half of it entirely. Best-effort,
                // GitHub-only (see `ForgeClient::github_failing_step`'s doc
                // comment for why GitLab needs no equivalent); a lookup
                // failure just means this check's paragraph reads the same
                // as before this enrichment existed.
                let enriched = FailedCheck {
                    failing_step: check
                        .job_url
                        .as_deref()
                        .and_then(|url| client.github_failing_step(url)),
                    ..check.clone()
                };
                describe_failing_check(
                    branch.worktree.as_deref(),
                    &enriched,
                    &reason_line,
                    only_one,
                )
            })
            .collect()
    };

    let prompt_body = format!(
        "{paragraphs}\n\n\
         The following are the prompts of the work already done on this branch -- keep this \
         behavior intact while fixing the CI failure:\n\n{cells}",
        paragraphs = paragraphs.join("\n\n"),
        cells = if cell_prompts.is_empty() {
            "(no cell prompts recorded for this branch)".to_string()
        } else {
            cell_prompts.join("\n\n---\n\n")
        },
    );

    let template = guardian
        .auto_fix_prompt_template
        .clone()
        .unwrap_or_else(|| crate::config::DEFAULT_AUTO_FIX_PROMPT_TEMPLATE.to_string());
    let pr_url = pr
        .pr_url
        .clone()
        .unwrap_or_else(|| format!("{} PR/MR #{}", pr.forge, pr.pr_number.unwrap_or_default()));
    let feedback = template.replace("{insert URL here}", &pr_url).replace(
        ralphus_core::validate::AUTO_FIX_PROMPT_PLACEHOLDER,
        &prompt_body,
    );

    // RAL-395 addendum (RAL-<new>: `author`/`submitted_by` now parameterized
    // rather than always `AUTO_FIX_AUTHOR`, so a manual dispatch attributes
    // to the actual person instead): post this round into the branch's
    // feedback thread the same way `guardian_merge::start_feedback` does for
    // a human reviewer -- so the board's chat thread shows *who* asked for
    // this change, not just that one happened. Superseding any still-
    // `received` pending message first mirrors `start_feedback`'s own
    // invariant: an older bubble must never read as in-progress once this
    // round has overtaken it. Best-effort -- a message-store failure must
    // never block the fix itself from running.
    let _ = store
        .lock()
        .supersede_pending_branch_feedback(&guardian.id, branch_id);
    let message_seq = store
        .lock()
        .add_guardian_message(
            &guardian.id,
            "reviewer",
            &feedback,
            None,
            Some(branch_id),
            Some(author),
            submitted_by,
        )
        .ok();

    log_ci_watch(
        store,
        &guardian.id,
        branch_id,
        LogLevel::INFO,
        format!(
            "ralphus [ci-watch] review {} branch {} auto-fix dispatching for pr #{}",
            guardian.id,
            branch_id,
            pr.pr_number.unwrap_or_default()
        ),
        serde_json::json!({"pr_number": pr.pr_number, "outcome": "auto_fix_dispatching"}),
    );
    let outcome = crate::guardian_merge::run_feedback(
        store,
        runner,
        &guardian.id,
        branch_id,
        &feedback,
        message_seq,
        true,
        &CancelToken::never(),
    );
    // `require_proof: true` above means `proof_passed` is only ever `None`
    // when `run_feedback` bailed out before the resolver agent ran at all
    // (e.g. a concurrent merge/rebuild had the branch's worktree torn down
    // at that instant) -- not a genuine "the agent tried and didn't fix it".
    // Give that case back its attempt so the next standing poll (or a later
    // manual retry) can try again, rather than letting a one-off
    // infrastructure race permanently disable auto-fix for a PR whose CI
    // never stops reporting "failing" in between.
    let Some(passed) = outcome.proof_passed else {
        let _ = store.lock().clear_pr_auto_fix_attempted(&pr.id);
        log_ci_watch(
            store,
            &guardian.id,
            branch_id,
            LogLevel::WARNING,
            format!(
                "ralphus [ci-watch] review {} branch {} auto-fix could not run for pr #{} \
                 (branch not ready yet); will retry next pass",
                guardian.id,
                branch_id,
                pr.pr_number.unwrap_or_default()
            ),
            serde_json::json!({
                "pr_number": pr.pr_number,
                "outcome": "auto_fix_not_attempted",
            }),
        );
        return;
    };
    log_ci_watch(
        store,
        &guardian.id,
        branch_id,
        if passed {
            LogLevel::INFO
        } else {
            LogLevel::WARNING
        },
        format!(
            "ralphus [ci-watch] review {} branch {} auto-fix {} for pr #{}",
            guardian.id,
            branch_id,
            if passed {
                "succeeded"
            } else {
                "did not confirm a fix"
            },
            pr.pr_number.unwrap_or_default()
        ),
        serde_json::json!({
            "pr_number": pr.pr_number,
            "outcome": if passed { "auto_fix_passed" } else { "auto_fix_failed" },
            "committed": outcome.committed,
            "pushed": outcome.pushed,
        }),
    );
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
    fn start_ci_watch_is_a_noop_without_an_open_pr() {
        let store = Arc::new(crate::store_lock::StoreMutex::new(
            Store::open_in_memory().unwrap(),
        ));
        let root = std::env::temp_dir();
        let guardian_id = {
            let guard = store.lock();
            guard
                .create_guardian("r", "main", &root.to_string_lossy())
                .unwrap()
        };
        let branch_id = {
            let guard = store.lock();
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
        start_ci_watch(&store, &guardian_id, &branch_id);
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

    fn failing(check_name: &str) -> PrCiState {
        PrCiState::Failing(PrFailure {
            reason: format!("check '{check_name}' failed"),
            job_url: None,
            log_text: None,
            checks: vec![],
        })
    }

    /// Two-branch stack (`feature/a` at position 0, `feature/b` at position 1,
    /// built on top of it), each with an open PR -- the fixture every
    /// `plan_auto_fix_dispatch` test below shares.
    fn two_branch_stack_with_open_prs(
        store: &crate::store_lock::StoreHandle,
    ) -> (GuardianView, PullRequestView, PullRequestView) {
        let guard = store.lock();
        let id = guard.create_guardian("r", "main", "/tmp/x").unwrap();
        guard.add_guardian_branch(&id, "feature/a").unwrap();
        guard.add_guardian_branch(&id, "feature/b").unwrap();
        let guardian = guard.get_guardian(&id).unwrap();
        let bid0 = guardian.branches[0].id.clone();
        let bid1 = guardian.branches[1].id.clone();
        let pr0_id = guard
            .create_pull_request(
                &id,
                Some(&bid0),
                "github",
                "acme/w",
                "a-alias",
                "main",
                "T",
                "",
                Some(1),
                None,
            )
            .unwrap();
        let pr1_id = guard
            .create_pull_request(
                &id,
                Some(&bid1),
                "github",
                "acme/w",
                "b-alias",
                "main",
                "T",
                "",
                Some(2),
                None,
            )
            .unwrap();
        let pr0 = guard.get_pull_request(&pr0_id).unwrap();
        let pr1 = guard.get_pull_request(&pr1_id).unwrap();
        (guardian, pr0, pr1)
    }

    #[test]
    fn plan_auto_fix_dispatch_defers_a_downstream_pr_while_its_upstream_is_failing() {
        let store = Arc::new(crate::store_lock::StoreMutex::new(
            Store::open_in_memory().unwrap(),
        ));
        let (guardian, pr0, pr1) = two_branch_stack_with_open_prs(&store);
        // Order in `polled` deliberately reversed from stack order -- the
        // downstream PR happened to poll first this pass -- to prove the
        // decision is driven by branch position, not poll/iteration order.
        let polled = vec![(pr1.clone(), failing("b")), (pr0.clone(), failing("a"))];
        let decisions = plan_auto_fix_dispatch(&guardian, &polled);
        assert_eq!(decisions.len(), 2);
        let upstream = decisions.iter().find(|d| d.pr.id == pr0.id).unwrap();
        let downstream = decisions.iter().find(|d| d.pr.id == pr1.id).unwrap();
        assert!(
            upstream.dispatch,
            "the most-upstream failing PR must be dispatched"
        );
        assert!(
            !downstream.dispatch,
            "a downstream PR must be deferred while an earlier-position branch's PR is still failing"
        );
    }

    #[test]
    fn plan_auto_fix_dispatch_allows_a_downstream_pr_once_its_upstream_is_clean() {
        let store = Arc::new(crate::store_lock::StoreMutex::new(
            Store::open_in_memory().unwrap(),
        ));
        let (guardian, pr0, pr1) = two_branch_stack_with_open_prs(&store);
        let polled = vec![
            (pr0.clone(), PrCiState::Passing),
            (pr1.clone(), failing("b")),
        ];
        let decisions = plan_auto_fix_dispatch(&guardian, &polled);
        assert_eq!(
            decisions.len(),
            1,
            "a passing upstream PR is not itself a decision"
        );
        assert!(
            decisions[0].dispatch,
            "a failing downstream PR must dispatch once its upstream is no longer failing"
        );
        assert_eq!(decisions[0].pr.id, pr1.id);
    }
}
