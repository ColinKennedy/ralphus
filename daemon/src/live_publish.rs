//! Publishing live-test results into the descriptions of a review's PRs/MRs.
//!
//! [`crate::live_publish::results`] renders the managed `## Live-Test Results` section;
//! this module decides which PRs get which actions and writes the section
//! through the forge REST API. A publish is a row in `live_result_refreshes`
//! (one per PR), so it survives a daemon restart and is retried with bounded
//! exponential backoff independently of any live run. The section is rendered
//! when the row is processed, from the stored run rows, so a request that
//! arrives while another is queued needs no second row.
//!
//! Publishing replaces only the text between the section markers in the body
//! the forge returns right before the update; the confirmed body is stored in
//! `guardian_pull_requests.description` only after the forge accepted it.

use std::path::Path;

use crate::live_actions::LiveAction;
use crate::live_publish::results::{
    ActionEntry, MAX_RUNS_PER_ACTION, merge_section, render_section,
};
use crate::live_runs::{LiveRunRow, guardian_live_runs_conn};
use crate::pr::PullRequestView;
use crate::store_lock::StoreHandle;

/// First retry delay; each further failure doubles it.
const BACKOFF_BASE_MS: i64 = 30_000;
/// The longest wait between two attempts on one PR.
const BACKOFF_CAP_MS: i64 = 60 * 60 * 1000;
/// Failures after which the review's watchers are told a second time.
const ESCALATE_AFTER_ATTEMPTS: i64 = 5;

/// Delay before the next attempt after `attempts` consecutive failures.
#[must_use]
pub fn backoff_ms(attempts: i64) -> i64 {
    let exp = u32::try_from((attempts - 1).clamp(0, 20)).unwrap_or(20);
    BACKOFF_BASE_MS
        .saturating_mul(1_i64 << exp)
        .min(BACKOFF_CAP_MS)
}

/// One PR's publication state, for the live-actions view.
#[derive(Debug, Clone, serde::Serialize, PartialEq)]
pub struct PublishStatus {
    pub pr_id: String,
    pub pr_number: Option<i64>,
    pub attempts: i64,
    pub next_attempt_ms: i64,
    pub last_error: Option<String>,
}

impl crate::store::Store {
    /// Queue a description refresh for every open PR of the review. A PR that
    /// already has a queued refresh keeps its row (and its backoff).
    ///
    /// # Errors
    ///
    /// On a database failure.
    pub fn enqueue_live_result_refresh(&self, guardian_id: &str) -> crate::store::Result<usize> {
        let now = crate::store::now_ms();
        Ok(self.conn.execute(
            "INSERT OR IGNORE INTO live_result_refreshes
                 (pr_id, guardian_id, attempts, next_attempt_ms, last_error, requested_at_ms)
             SELECT id, guardian_id, 0, ?1, NULL, ?1 FROM guardian_pull_requests
             WHERE guardian_id=?2 AND state='open' AND pr_number IS NOT NULL",
            rusqlite::params![now, guardian_id],
        )?)
    }

    /// Make every queued refresh of the review due now (the manual retry).
    ///
    /// # Errors
    ///
    /// On a database failure.
    pub fn reset_live_result_refresh_backoff(
        &self,
        guardian_id: &str,
    ) -> crate::store::Result<usize> {
        Ok(self.conn.execute(
            "UPDATE live_result_refreshes SET next_attempt_ms=0 WHERE guardian_id=?",
            rusqlite::params![guardian_id],
        )?)
    }

    /// Queued (not yet published) refreshes of the review.
    ///
    /// # Errors
    ///
    /// On a database failure.
    pub fn live_result_publish_status(
        &self,
        guardian_id: &str,
    ) -> crate::store::Result<Vec<PublishStatus>> {
        let mut stmt = self.conn.prepare(
            "SELECT f.pr_id, p.pr_number, f.attempts, f.next_attempt_ms, f.last_error
             FROM live_result_refreshes f
             JOIN guardian_pull_requests p ON p.id = f.pr_id
             WHERE f.guardian_id=? ORDER BY p.created_at_ms, p.id",
        )?;
        let rows = stmt.query_map(rusqlite::params![guardian_id], |r| {
            Ok(PublishStatus {
                pr_id: r.get(0)?,
                pr_number: r.get(1)?,
                attempts: r.get(2)?,
                next_attempt_ms: r.get(3)?,
                last_error: r.get(4)?,
            })
        })?;
        Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
    }

    fn due_live_result_refreshes(&self, now: i64) -> Vec<(String, String, i64)> {
        let Ok(mut stmt) = self.conn.prepare(
            "SELECT pr_id, guardian_id, attempts FROM live_result_refreshes
             WHERE next_attempt_ms <= ? ORDER BY requested_at_ms, pr_id",
        ) else {
            return Vec::new();
        };
        stmt.query_map(rusqlite::params![now], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?))
        })
        .map(|rows| rows.flatten().collect())
        .unwrap_or_default()
    }

    fn clear_live_result_refresh(&self, pr_id: &str) {
        let _ = self.conn.execute(
            "DELETE FROM live_result_refreshes WHERE pr_id=?",
            rusqlite::params![pr_id],
        );
    }

    fn fail_live_result_refresh(&self, pr_id: &str, attempts: i64, error: &str) {
        let _ = self.conn.execute(
            "UPDATE live_result_refreshes SET attempts=?, next_attempt_ms=?, last_error=?
             WHERE pr_id=?",
            rusqlite::params![
                attempts,
                crate::store::now_ms().saturating_add(backoff_ms(attempts)),
                error,
                pr_id
            ],
        );
    }
}

/// Queue a description refresh for the review's open PRs.
pub fn request_refresh(store: &StoreHandle, guardian_id: &str) {
    let _ = store.lock().enqueue_live_result_refresh(guardian_id);
}

/// The PR that carries the review-wide entries: the combined-worktree PR if
/// there is one, otherwise the PR of the highest-position branch.
fn top_pr_id<'a>(
    prs: &'a [PullRequestView],
    positions: &std::collections::HashMap<&str, i64>,
) -> Option<&'a str> {
    prs.iter()
        .max_by_key(|p| match p.branch_id.as_deref() {
            None => i64::MAX,
            Some(id) => positions.get(id).copied().unwrap_or(-1),
        })
        .map(|p| p.id.as_str())
}

/// The entries one PR shows: its branch's actions, plus (on the top PR only)
/// the actions with no resolved branch, marked review-wide. Newest
/// [`MAX_RUNS_PER_ACTION`] runs per action; user-authored action runs and runs
/// that have not started or were cancelled are not entries.
#[must_use]
pub fn entries_for_pr(
    pr: &PullRequestView,
    is_top: bool,
    actions: &[LiveAction],
    runs: &[LiveRunRow],
) -> Vec<ActionEntry> {
    let mut entries = Vec::new();
    for action in actions {
        let attributed = !action.branch_ids.is_empty();
        let on_this_pr = match pr.branch_id.as_deref() {
            None => true,
            Some(id) => action.branch_ids.iter().any(|b| b == id),
        };
        let review_wide = !attributed;
        if (attributed && !on_this_pr) || (review_wide && !is_top) {
            continue;
        }
        let action_runs: Vec<LiveRunRow> = runs
            .iter()
            .filter(|r| r.kind == "live" && r.action_idx == action.idx)
            .filter(|r| !matches!(r.state.as_str(), "queued" | "running" | "cancelled"))
            .take(MAX_RUNS_PER_ACTION)
            .cloned()
            .collect();
        if action_runs.is_empty() {
            continue;
        }
        entries.push(ActionEntry {
            action: action.clone(),
            runs: action_runs,
            review_wide,
        });
    }
    entries
}

enum Outcome {
    Published,
    Nothing,
}

/// Publish every refresh that is due. Never holds the store lock across a
/// forge call.
pub fn process_due_refreshes(store: &StoreHandle) {
    let due = store
        .lock()
        .due_live_result_refreshes(crate::store::now_ms());
    for (pr_id, guardian_id, attempts) in due {
        match publish_pr(store, &guardian_id, &pr_id) {
            Ok(Outcome::Published | Outcome::Nothing) => {
                store.lock().clear_live_result_refresh(&pr_id)
            }
            Err(error) => record_failure(store, &guardian_id, &pr_id, attempts + 1, &error),
        }
    }
}

/// The manual retry: queue, make due, and publish the review's PRs now.
pub fn retry_now(store: &StoreHandle, guardian_id: &str) {
    {
        let guard = store.lock();
        let _ = guard.enqueue_live_result_refresh(guardian_id);
        let _ = guard.reset_live_result_refresh_backoff(guardian_id);
    }
    process_due_refreshes(store);
}

fn publish_pr(store: &StoreHandle, guardian_id: &str, pr_id: &str) -> Result<Outcome, String> {
    let (guardian, prs, actions, runs) = {
        let guard = store.lock();
        let guardian = guard.get_guardian(guardian_id).map_err(|e| e.to_string())?;
        let prs = guard
            .list_pull_requests_for_guardian(guardian_id)
            .map_err(|e| e.to_string())?;
        let actions = guard
            .guardian_live_actions(guardian_id)
            .map_err(|e| e.to_string())?;
        let runs = guardian_live_runs_conn(&guard.conn, guardian_id).map_err(|e| e.to_string())?;
        (guardian, prs, actions, runs)
    };
    let open: Vec<PullRequestView> = prs.into_iter().filter(|p| p.state == "open").collect();
    let Some(pr) = open.iter().find(|p| p.id == pr_id) else {
        return Ok(Outcome::Nothing);
    };
    let Some(number) = pr.pr_number else {
        return Ok(Outcome::Nothing);
    };
    let positions: std::collections::HashMap<&str, i64> = guardian
        .branches
        .iter()
        .map(|b| (b.id.as_str(), b.position))
        .collect();
    let is_top = top_pr_id(&open, &positions) == Some(pr.id.as_str());
    let entries = entries_for_pr(pr, is_top, &actions, &runs);
    let Some(section) = render_section(&entries) else {
        return Ok(Outcome::Nothing);
    };
    let root = Path::new(&guardian.git_root);
    let forge_cfg = crate::config::resolve_forge(root);
    let client = crate::pr::forge_client_for_pr(
        store,
        root,
        &guardian.base_branch,
        &forge_cfg,
        &pr.repo,
        guardian.owner.as_deref(),
    )
    .ok_or_else(|| format!("no forge client for {}", pr.repo))?;
    let current = client.get_pull_request_description(number)?;
    let merged = merge_section(&current, &section);
    if merged != current {
        client.update_pull_request_description(number, &merged)?;
    }
    let _ = store.lock().set_pr_description(&pr.id, &merged);
    crate::cartographer::Note::new("guardian")
        .guardian(guardian_id)
        .scope("live_action")
        .emit(
            &store.lock(),
            "live-test results published to PR description",
            serde_json::json!({ "pr_id": pr.id, "pr_number": number, "repo": pr.repo }),
        );
    Ok(Outcome::Published)
}

fn record_failure(store: &StoreHandle, guardian_id: &str, pr_id: &str, attempts: i64, error: &str) {
    crate::rlog!(
        WARNING,
        "ralphus [guardian] review {guardian_id} live-test results for PR {pr_id} not published (attempt {attempts}): {error}"
    );
    let guard = store.lock();
    guard.fail_live_result_refresh(pr_id, attempts, error);
    crate::cartographer::Note::new("guardian")
        .guardian(guardian_id)
        .scope("live_action")
        .emit(
            &guard,
            "live-test results publish failed",
            serde_json::json!({
                "pr_id": pr_id,
                "attempts": attempts,
                "error": error,
                "retry_in_ms": backoff_ms(attempts),
            }),
        );
    if attempts == 1 || attempts == ESCALATE_AFTER_ATTEMPTS {
        let _ = guard.enqueue_error_mailbox_message(
            crate::mailbox::MailboxPriority::Normal,
            &format!(
                "Live-test results could not be written to a PR description of review \
                 {guardian_id} (attempt {attempts}): {error}"
            ),
            &crate::mailbox::Remediation::ManualInterventionRequired {
                guidance: format!(
                    "the daemon retries automatically with growing delays; once the forge \
                     is reachable and the token can edit the PR, retry now with \
                     `POST /api/guardians/{guardian_id}/live-actions/results/refresh` or the \
                     Retry control in the review's live actions"
                ),
            },
            None,
            None,
            None,
            Some(&format!("guardian:{guardian_id}")),
            Some("live_actions"),
        );
    }
}

pub mod results;
#[cfg(test)]
mod tests;
