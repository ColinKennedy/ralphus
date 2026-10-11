//! Forge access health: turns a stream of failing forge calls into one
//! mailbox notice per outage.
//!
//! Every background poller (PR merge detection, CI status, stack reorder
//! checks, comment polling, remote-commit sync) treats a forge failure as
//! "assume nothing changed" so an unreachable forge is never mistaken for a
//! merged PR. That is the right data policy, but on its own it is silent: a
//! revoked token or an exhausted rate limit leaves every review looking
//! healthy while nothing is being detected. Those callers report here so the
//! user hears about it once, with a next step, and not once per poll.
//!
//! State is in memory only. An outage is keyed by `forge:repo`; the first
//! report of a problem notifies, further reports stay quiet until a success
//! for that key clears it ([`note_success`]) or [`REMIND_AFTER`] elapses. A
//! daemon restart therefore re-notifies an outage that is still going, which
//! is the behavior we want after a restart.

use std::collections::HashMap;
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, Instant};

use crate::mailbox::{MailboxPriority, Remediation};

/// How long an unreachable-forge condition (network error, 5xx) must last
/// without a single success before it is worth a notice. Auth and rate-limit
/// failures notify at once: they never fix themselves quickly and a retry
/// storm makes a rate limit worse.
pub const TRANSIENT_NOTIFY_AFTER: Duration = Duration::from_secs(10 * 60);

/// An outage that is still going this long after its last notice is reported
/// again, so a dismissed notice cannot hide a day-long outage.
pub const REMIND_AFTER: Duration = Duration::from_secs(6 * 60 * 60);

/// What kind of forge access problem a failed call represents.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Problem {
    /// 401: the token was rejected or is missing.
    Unauthorized,
    /// 429, or a 403 whose body says the rate limit was exceeded.
    RateLimited,
    /// Any other 403: the token is valid but lacks access to the resource.
    Forbidden,
    /// No HTTP answer at all (timeout, DNS, TLS) or a 5xx.
    Unreachable,
}

/// The HTTP status carried by a stringified forge error
/// (`"forge API 403: ..."`), or `None` for a transport error
/// (`"forge API: ..."`) and anything else.
#[must_use]
pub fn status_from_message(message: &str) -> Option<u16> {
    message
        .strip_prefix("forge API ")?
        .split(':')
        .next()?
        .trim()
        .parse()
        .ok()
}

/// Classify a failed forge call. `None` means the failure says nothing about
/// our access to the forge (a 404 for a deleted PR, a 422 validation error,
/// a JSON parse error), so it is not an outage.
#[must_use]
pub fn classify(status: Option<u16>, message: &str) -> Option<Problem> {
    match status {
        Some(401) => Some(Problem::Unauthorized),
        Some(429) => Some(Problem::RateLimited),
        Some(403) => {
            let lowered = message.to_ascii_lowercase();
            if lowered.contains("rate limit") || lowered.contains("abuse detection") {
                Some(Problem::RateLimited)
            } else {
                Some(Problem::Forbidden)
            }
        }
        Some(500..=599) => Some(Problem::Unreachable),
        Some(_) => None,
        None if message.starts_with("forge API:") => Some(Problem::Unreachable),
        None => None,
    }
}

/// One failed forge call, as reported by a poller.
pub struct FailureReport<'a> {
    /// `"github"` / `"gitlab"`.
    pub forge: &'a str,
    /// The `owner/repo` (or encoded GitLab path) the call was against.
    pub repo: &'a str,
    /// The review the call was made for, when there is one; the notice is
    /// then addressed to it.
    pub guardian_id: Option<&'a str>,
    /// What the daemon was doing, e.g. "PR merge detection".
    pub activity: &'a str,
    /// The stringified forge error (`"forge API 403: ..."`).
    pub error: &'a str,
    /// The forge's own `Retry-After`, when it sent one.
    pub retry_after: Option<Duration>,
}

struct Outage {
    problem: Problem,
    first_failure: Instant,
    last_notified: Option<Instant>,
}

static OUTAGES: LazyLock<Mutex<HashMap<String, Outage>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

fn outage_key(forge: &str, repo: &str) -> String {
    format!("{forge}:{repo}")
}

/// Decide whether this failure should notify, updating `outages`. Pure over
/// its arguments so the timing rules are testable without sleeping.
fn should_notify(
    outages: &mut HashMap<String, Outage>,
    key: &str,
    problem: Problem,
    now: Instant,
) -> bool {
    let outage = outages
        .entry(key.to_string())
        .and_modify(|existing| {
            if existing.problem != problem {
                *existing = Outage {
                    problem,
                    first_failure: now,
                    last_notified: None,
                };
            }
        })
        .or_insert(Outage {
            problem,
            first_failure: now,
            last_notified: None,
        });
    let due = problem != Problem::Unreachable
        || now.duration_since(outage.first_failure) >= TRANSIENT_NOTIFY_AFTER;
    if !due {
        return false;
    }
    match outage.last_notified {
        None => {}
        Some(last) if now.duration_since(last) >= REMIND_AFTER => {}
        Some(_) => return false,
    }
    outage.last_notified = Some(now);
    true
}

/// A forge call for `forge`/`repo` succeeded: whatever outage was open is
/// over, and the next failure notifies again.
pub fn note_success(forge: &str, repo: &str) {
    OUTAGES
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .remove(&outage_key(forge, repo));
}

/// Report a failed forge call. Enqueues a mailbox notice when this starts (or
/// re-reminds about) an outage; returns whether it did. Failures that are not
/// an access problem ([`classify`] returns `None`) are ignored.
pub fn report_failure(store: &crate::store_lock::StoreHandle, report: &FailureReport<'_>) -> bool {
    let status = status_from_message(report.error);
    let Some(problem) = classify(status, report.error) else {
        return false;
    };
    let notify = {
        let mut outages = OUTAGES
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        should_notify(
            &mut outages,
            &outage_key(report.forge, report.repo),
            problem,
            Instant::now(),
        )
    };
    if !notify {
        return false;
    }
    let (message, remediation) = describe(problem, status, report);
    let entity_uri = report.guardian_id.map(|id| format!("guardian:{id}"));
    let category = if report.guardian_id.is_some() {
        "review"
    } else {
        "forge"
    };
    let result = store.lock().enqueue_error_mailbox_message(
        MailboxPriority::High,
        &message,
        &remediation,
        None,
        None,
        None,
        entity_uri.as_deref(),
        Some(category),
    );
    if let Err(error) = result {
        crate::rlog!(
            WARNING,
            "ralphus [forge] could not enqueue the {} {} access notice: {error}",
            report.forge,
            report.repo
        );
        // Not delivered: forget the attempt so the next failure retries it.
        note_success(report.forge, report.repo);
        return false;
    }
    true
}

/// [`report_failure`] for a caller that only has the stringified error.
pub fn report_error(
    store: &crate::store_lock::StoreHandle,
    guardian_id: Option<&str>,
    forge: &str,
    repo: &str,
    activity: &str,
    error: &str,
) -> bool {
    report_failure(
        store,
        &FailureReport {
            forge,
            repo,
            guardian_id,
            activity,
            error,
            retry_after: None,
        },
    )
}

fn describe(
    problem: Problem,
    status: Option<u16>,
    report: &FailureReport<'_>,
) -> (String, Remediation) {
    let target = format!("{} {}", report.forge, report.repo);
    let activity = report.activity;
    match problem {
        Problem::RateLimited => {
            let wait = report.retry_after.map_or_else(
                || "the forge did not say how long".to_string(),
                |d| format!("the forge asked for {}s", d.as_secs()),
            );
            (
                format!(
                    "{target} is rate limiting the daemon (HTTP {}): {activity} is paused and \
                     will not notice merged PRs or CI results until the limit resets \
                     ({wait}).",
                    status.unwrap_or(429)
                ),
                Remediation::ManualInterventionRequired {
                    guidance: "wait for the rate-limit window to reset; detection resumes by \
                               itself. If it keeps recurring, many reviews are polling the \
                               same token, so give the forge a token with a higher limit \
                               (`ralphus user set-forge-token`)"
                        .to_string(),
                },
            )
        }
        Problem::Unauthorized => (
            format!(
                "{target} rejected the daemon's token (HTTP 401): {activity} cannot run, so \
                 merged PRs and CI results will not be noticed."
            ),
            Remediation::ManualInterventionRequired {
                guidance: "the token is expired or revoked; replace it with \
                           `ralphus user set-forge-token` (check it afterwards with \
                           `ralphus user check-forge-token`) or re-run the forge CLI login the \
                           daemon falls back to"
                    .to_string(),
            },
        ),
        Problem::Forbidden => (
            format!(
                "{target} refused the daemon access (HTTP 403, not a rate limit): {activity} \
                 cannot run, so merged PRs and CI results will not be noticed."
            ),
            Remediation::ManualInterventionRequired {
                guidance: "the token lacks access to this repository; grant it repository \
                           and pull-request read access (or SSO authorization), then verify \
                           with `ralphus user check-forge-token`"
                    .to_string(),
            },
        ),
        Problem::Unreachable => (
            format!(
                "{target} has been failing for over {} minutes ({}): {activity} is not \
                 getting answers, so merged PRs and CI results will not be noticed.",
                TRANSIENT_NOTIFY_AFTER.as_secs() / 60,
                status.map_or_else(|| "no response".to_string(), |s| format!("HTTP {s}")),
            ),
            Remediation::ManualInterventionRequired {
                guidance: "check the forge's status page and this machine's network; the \
                           daemon retries on its own and this notice clears once a call \
                           succeeds"
                    .to_string(),
            },
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::Store;
    use std::sync::Arc;

    fn handle() -> crate::store_lock::StoreHandle {
        Arc::new(crate::store_lock::StoreMutex::new(
            Store::open_in_memory().unwrap(),
        ))
    }

    fn messages(store: &crate::store_lock::StoreHandle) -> Vec<String> {
        let guard = store.lock();
        let client = guard.register_mailbox_client().unwrap();
        guard
            .mailbox_messages_for_client_filtered(&client, false, None, None)
            .unwrap()
            .into_iter()
            .map(|m| m.message)
            .collect()
    }

    fn report<'a>(repo: &'a str, error: &'a str) -> FailureReport<'a> {
        FailureReport {
            forge: "github",
            repo,
            guardian_id: Some("guardian-000000000001"),
            activity: "PR merge detection",
            error,
            retry_after: None,
        }
    }

    const RATE_LIMITED: &str = "forge API 403: {\"message\": \"API rate limit exceeded for user\"}";

    #[test]
    fn status_is_read_from_the_stringified_error() {
        assert_eq!(status_from_message("forge API 403: {}"), Some(403));
        assert_eq!(status_from_message("forge API 502: bad gateway"), Some(502));
        assert_eq!(status_from_message("forge API: Connection Failed"), None);
        assert_eq!(status_from_message("something else"), None);
    }

    #[test]
    fn classify_separates_rate_limit_from_other_forbidden() {
        assert_eq!(
            classify(Some(401), "forge API 401: Bad credentials"),
            Some(Problem::Unauthorized)
        );
        assert_eq!(
            classify(Some(429), "forge API 429:"),
            Some(Problem::RateLimited)
        );
        assert_eq!(
            classify(Some(403), RATE_LIMITED),
            Some(Problem::RateLimited)
        );
        assert_eq!(
            classify(
                Some(403),
                "forge API 403: Resource not accessible by integration"
            ),
            Some(Problem::Forbidden)
        );
        assert_eq!(
            classify(Some(503), "forge API 503:"),
            Some(Problem::Unreachable)
        );
        assert_eq!(
            classify(None, "forge API: Connection Failed"),
            Some(Problem::Unreachable)
        );
    }

    #[test]
    fn classify_ignores_failures_that_are_not_access_problems() {
        assert_eq!(classify(Some(404), "forge API 404: Not Found"), None);
        assert_eq!(classify(Some(422), "forge API 422: invalid"), None);
        assert_eq!(classify(None, "forge API JSON parse: eof"), None);
    }

    #[test]
    fn a_rate_limit_notifies_once_then_stays_quiet() {
        let mut outages = HashMap::new();
        let now = Instant::now();
        assert!(should_notify(&mut outages, "k", Problem::RateLimited, now));
        let later = now + Duration::from_secs(120);
        assert!(!should_notify(
            &mut outages,
            "k",
            Problem::RateLimited,
            later
        ));
    }

    #[test]
    fn an_outage_is_reminded_after_the_reminder_interval() {
        let mut outages = HashMap::new();
        let now = Instant::now();
        assert!(should_notify(&mut outages, "k", Problem::Unauthorized, now));
        assert!(!should_notify(
            &mut outages,
            "k",
            Problem::Unauthorized,
            now + REMIND_AFTER - Duration::from_secs(1)
        ));
        assert!(should_notify(
            &mut outages,
            "k",
            Problem::Unauthorized,
            now + REMIND_AFTER
        ));
    }

    #[test]
    fn a_transient_failure_only_notifies_once_it_has_lasted() {
        let mut outages = HashMap::new();
        let now = Instant::now();
        assert!(!should_notify(&mut outages, "k", Problem::Unreachable, now));
        assert!(!should_notify(
            &mut outages,
            "k",
            Problem::Unreachable,
            now + TRANSIENT_NOTIFY_AFTER - Duration::from_secs(1)
        ));
        assert!(should_notify(
            &mut outages,
            "k",
            Problem::Unreachable,
            now + TRANSIENT_NOTIFY_AFTER
        ));
    }

    #[test]
    fn a_different_problem_for_the_same_repo_notifies_again() {
        let mut outages = HashMap::new();
        let now = Instant::now();
        assert!(should_notify(&mut outages, "k", Problem::RateLimited, now));
        assert!(should_notify(&mut outages, "k", Problem::Unauthorized, now));
    }

    #[test]
    fn report_failure_enqueues_one_notice_with_remediation_and_dedupes() {
        let store = handle();
        let repo = "acme/forge-health-dedupe";
        assert!(report_failure(&store, &report(repo, RATE_LIMITED)));
        assert!(!report_failure(&store, &report(repo, RATE_LIMITED)));
        let all = messages(&store);
        assert_eq!(all.len(), 1, "{all:?}");
        assert!(all[0].contains("rate limiting"), "{}", all[0]);
        assert!(
            all[0].contains("Manual intervention required:"),
            "{}",
            all[0]
        );
        note_success("github", repo);
    }

    #[test]
    fn a_success_re_arms_the_notice() {
        let store = handle();
        let repo = "acme/forge-health-rearm";
        assert!(report_failure(&store, &report(repo, RATE_LIMITED)));
        note_success("github", repo);
        assert!(report_failure(&store, &report(repo, RATE_LIMITED)));
        assert_eq!(messages(&store).len(), 2);
        note_success("github", repo);
    }

    #[test]
    fn a_non_access_failure_never_notifies() {
        let store = handle();
        assert!(!report_failure(
            &store,
            &report("acme/forge-health-404", "forge API 404: Not Found")
        ));
        assert!(messages(&store).is_empty());
    }
}
