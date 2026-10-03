//! Observable health tracking for periodic upstream-branch polls (RAL-545):
//! the base-branch freshness fetch (`guardian_merge::poll_base_branch_freshness_once`)
//! and the PR/MR forge refresh (`pr::refresh_pr_forge_cache_for_guardian`).
//!
//! Health is persisted in SQLite as a durable healthy/unhealthy *transition*
//! per `(poller_kind, target_key)` -- never a retry counter -- so a daemon
//! restart can never replay an outage notification that already fired.
//! Repeated same-state polls are logged to Cartographer with exponential
//! backoff to avoid noise; a transition (healthy->unhealthy or
//! unhealthy->healthy) always resets the backoff and logs immediately, so
//! the first event in the new state is visible. The first failure after a
//! healthy state additionally raises a `High`-priority mailbox notification
//! with remediation; repeated failures while already unhealthy raise no
//! further notification, and recovery raises none either.
//!
//! `target_key` is a guardian (review) id today for both poller kinds --
//! `base_branch_fetch` targets are deduped across guardians for the actual
//! git fetch (see `guardian_merge::collect_base_fetch_targets`), but health
//! is attributed back to every guardian that target serves, since that is
//! the unit a user watches and the mailbox notifies against.

use rusqlite::OptionalExtension as _;

use crate::store::{Result, Store, now_ms};

/// Repeated same-state health logs double this many polls apart, capped here
/// so an indefinitely long outage/healthy streak still logs at least this
/// often rather than falling silent forever.
const LOG_BACKOFF_CAP_STREAK: i64 = 64;

/// Which periodic upstream poll a health row describes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PollerKind {
    /// `guardian_merge::poll_base_branch_freshness_once`'s local-ref refresh.
    BaseBranchFetch,
    /// `pr::refresh_pr_forge_cache_for_guardian`'s forge drift/comment poll.
    PrForge,
}

impl PollerKind {
    /// Stable lowercase string stored in the database.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::BaseBranchFetch => "base_branch_fetch",
            Self::PrForge => "pr_forge",
        }
    }

    /// Human-readable label used in log/mailbox message text.
    fn label(self) -> &'static str {
        match self {
            Self::BaseBranchFetch => "upstream base-branch fetch",
            Self::PrForge => "PR/MR forge",
        }
    }
}

/// The result of one poll attempt against a target, as reported by the
/// caller to [`Store::record_poll_outcome`].
#[derive(Debug, Clone)]
pub enum PollOutcome {
    Healthy,
    Unhealthy { error: String },
}

impl PollOutcome {
    fn is_healthy(&self) -> bool {
        matches!(self, Self::Healthy)
    }
}

/// Persisted health state for one `(poller_kind, target_key)`.
#[derive(Debug, Clone, PartialEq, Eq)]
struct PollerHealthRow {
    healthy: bool,
    streak: i64,
    next_log_at_streak: i64,
}

/// What [`decide`] determined should happen for one poll outcome: whether to
/// emit a Cartographer log this time, whether a healthy<->unhealthy
/// transition just happened (callers use this to gate the mailbox
/// notification), and the new streak/backoff state to persist.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct PollDecision {
    should_log: bool,
    transitioned_to_unhealthy: bool,
    transitioned_to_healthy: bool,
    new_streak: i64,
    new_next_log_at_streak: i64,
}

/// Pure decision logic: given the previous persisted state (`None` if this
/// target has never been recorded) and whether this poll was healthy, decide
/// whether to log and what the new streak/backoff state should be.
///
/// A transition always logs and resets the streak to 1 / next-log-at to 2.
/// A repeat of the same state only logs once its streak reaches
/// `next_log_at_streak`, at which point that threshold doubles (capped at
/// [`LOG_BACKOFF_CAP_STREAK`]) -- so repeat logs land at streak 1, 2, 4, 8,
/// 16, ... rather than every poll.
fn decide(prev: Option<&PollerHealthRow>, healthy: bool) -> PollDecision {
    match prev {
        None => PollDecision {
            should_log: true,
            transitioned_to_unhealthy: !healthy,
            transitioned_to_healthy: false,
            new_streak: 1,
            new_next_log_at_streak: 2,
        },
        Some(prev) if prev.healthy != healthy => PollDecision {
            should_log: true,
            transitioned_to_unhealthy: !healthy,
            transitioned_to_healthy: healthy,
            new_streak: 1,
            new_next_log_at_streak: 2,
        },
        Some(prev) => {
            let new_streak = prev.streak + 1;
            let should_log = new_streak >= prev.next_log_at_streak;
            let new_next_log_at_streak = if should_log {
                (prev.next_log_at_streak.saturating_mul(2)).min(LOG_BACKOFF_CAP_STREAK)
            } else {
                prev.next_log_at_streak
            };
            PollDecision {
                should_log,
                transitioned_to_unhealthy: false,
                transitioned_to_healthy: false,
                new_streak,
                new_next_log_at_streak,
            }
        }
    }
}

impl Store {
    /// Record the outcome of one poll attempt against `target_key` (today
    /// always a guardian id) for `kind`, persisting the health transition,
    /// emitting a Cartographer health log when the backoff policy calls for
    /// one, and -- only on the first failure right after a healthy state --
    /// raising a `High`-priority mailbox notification with remediation.
    ///
    /// # Errors
    /// Propagates any SQLite failure.
    pub fn record_poll_outcome(
        &self,
        kind: PollerKind,
        target_key: &str,
        outcome: &PollOutcome,
    ) -> Result<()> {
        let now = now_ms();
        let prev = self.get_poller_health(kind, target_key)?;
        let healthy = outcome.is_healthy();
        let decision = decide(prev.as_ref(), healthy);
        let last_error = match outcome {
            PollOutcome::Healthy => None,
            PollOutcome::Unhealthy { error } => Some(error.as_str()),
        };
        self.upsert_poller_health(
            kind,
            target_key,
            healthy,
            decision.new_streak,
            decision.new_next_log_at_streak,
            last_error,
            now,
        )?;

        if decision.should_log {
            let level = if healthy {
                crate::logging::LogLevel::INFO
            } else {
                crate::logging::LogLevel::WARNING
            };
            let message = match outcome {
                PollOutcome::Healthy if decision.transitioned_to_healthy => {
                    format!("{} poll recovered for review {target_key}", kind.label())
                }
                PollOutcome::Healthy => {
                    format!("{} poll healthy for review {target_key}", kind.label())
                }
                PollOutcome::Unhealthy { error } => format!(
                    "{} poll failing for review {target_key}: {error}",
                    kind.label()
                ),
            };
            crate::cartographer::Note::new("poller_health")
                .scope(kind.as_str())
                .guardian(target_key)
                .level(level)
                .emit(
                    self,
                    &message,
                    serde_json::json!({
                        "poller_kind": kind.as_str(),
                        "healthy": healthy,
                        "streak": decision.new_streak,
                    }),
                );
        }

        if decision.transitioned_to_unhealthy {
            if let PollOutcome::Unhealthy { error } = outcome {
                let message = format!(
                    "{} poll started failing for review {target_key}: {error}",
                    kind.label()
                );
                let remediation = crate::mailbox::Remediation::ManualInterventionRequired {
                    guidance: format!(
                        "check the git remote, network connectivity, and any personal access \
                         token used for this review's upstream -- {error} -- it will retry \
                         automatically on the next poll"
                    ),
                };
                let _ = self.notify_watchers_with_remediation(
                    crate::monitor::NotifiableEventKind::ReviewFailed,
                    &format!("guardian:{target_key}"),
                    crate::mailbox::MailboxPriority::High,
                    &message,
                    &remediation,
                    None,
                    None,
                    None,
                );
            }
        }
        Ok(())
    }

    fn get_poller_health(
        &self,
        kind: PollerKind,
        target_key: &str,
    ) -> Result<Option<PollerHealthRow>> {
        self.conn
            .query_row(
                "SELECT healthy, streak, next_log_at_streak FROM poller_health \
                 WHERE poller_kind=?1 AND target_key=?2",
                rusqlite::params![kind.as_str(), target_key],
                |r| {
                    Ok(PollerHealthRow {
                        healthy: r.get::<_, i64>(0)? != 0,
                        streak: r.get(1)?,
                        next_log_at_streak: r.get(2)?,
                    })
                },
            )
            .optional()
            .map_err(Into::into)
    }

    #[allow(clippy::too_many_arguments)]
    fn upsert_poller_health(
        &self,
        kind: PollerKind,
        target_key: &str,
        healthy: bool,
        streak: i64,
        next_log_at_streak: i64,
        last_error: Option<&str>,
        updated_at_ms: i64,
    ) -> Result<()> {
        self.conn.execute(
            "INSERT INTO poller_health(poller_kind, target_key, healthy, streak, next_log_at_streak, last_error, updated_at_ms)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
             ON CONFLICT(poller_kind, target_key) DO UPDATE SET
                healthy = excluded.healthy,
                streak = excluded.streak,
                next_log_at_streak = excluded.next_log_at_streak,
                last_error = excluded.last_error,
                updated_at_ms = excluded.updated_at_ms",
            rusqlite::params![
                kind.as_str(),
                target_key,
                i64::from(healthy),
                streak,
                next_log_at_streak,
                last_error,
                updated_at_ms
            ],
        )?;
        Ok(())
    }

    /// Clear every poller-health row for one review (RAL-545) -- called
    /// whenever its upstream base branch changes, since a stale
    /// unhealthy/backoff state recorded against the old upstream has no
    /// bearing on the new one.
    ///
    /// # Errors
    /// Propagates any SQLite failure.
    pub fn clear_poller_health_for_guardian(&self, guardian_id: &str) -> Result<()> {
        self.conn.execute(
            "DELETE FROM poller_health WHERE target_key=?1",
            rusqlite::params![guardian_id],
        )?;
        Ok(())
    }

    /// Clear every poller-health row across every review (RAL-545) --
    /// called whenever any personal access token changes, since the
    /// backoff/health state recorded so far may have been produced against
    /// stale credentials and no longer reflects the new token. Applied
    /// broadly rather than narrowed to the changed token's own user: a
    /// shared daemon can have multiple reviews whose polling depends on
    /// tokens other than the one that just changed, and there is no cheap,
    /// reliable way to know in advance which reviews those are.
    ///
    /// # Errors
    /// Propagates any SQLite failure.
    pub fn clear_all_poller_health(&self) -> Result<()> {
        self.conn.execute("DELETE FROM poller_health", [])?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(healthy: bool, streak: i64, next_log_at_streak: i64) -> PollerHealthRow {
        PollerHealthRow {
            healthy,
            streak,
            next_log_at_streak,
        }
    }

    #[test]
    fn first_ever_poll_always_logs() {
        let d = decide(None, true);
        assert!(d.should_log);
        assert!(!d.transitioned_to_unhealthy);
        assert!(!d.transitioned_to_healthy);
        assert_eq!(d.new_streak, 1);
        assert_eq!(d.new_next_log_at_streak, 2);

        let d = decide(None, false);
        assert!(d.should_log);
        assert!(d.transitioned_to_unhealthy);
    }

    #[test]
    fn healthy_to_unhealthy_transition_always_logs_and_resets() {
        let prev = row(true, 40, 64);
        let d = decide(Some(&prev), false);
        assert!(d.should_log);
        assert!(d.transitioned_to_unhealthy);
        assert!(!d.transitioned_to_healthy);
        assert_eq!(d.new_streak, 1);
        assert_eq!(d.new_next_log_at_streak, 2);
    }

    #[test]
    fn unhealthy_to_healthy_transition_always_logs_and_resets() {
        let prev = row(false, 10, 16);
        let d = decide(Some(&prev), true);
        assert!(d.should_log);
        assert!(!d.transitioned_to_unhealthy);
        assert!(d.transitioned_to_healthy);
        assert_eq!(d.new_streak, 1);
        assert_eq!(d.new_next_log_at_streak, 2);
    }

    #[test]
    fn repeated_same_state_backs_off_exponentially() {
        // streak 1 -> 2: next_log_at_streak was 2, 2 >= 2 so logs and doubles to 4.
        let prev = row(true, 1, 2);
        let d = decide(Some(&prev), true);
        assert!(d.should_log);
        assert_eq!(d.new_streak, 2);
        assert_eq!(d.new_next_log_at_streak, 4);

        // streak 2 -> 3: next_log_at_streak is 4, 3 < 4 so suppressed.
        let prev = row(true, 2, 4);
        let d = decide(Some(&prev), true);
        assert!(!d.should_log);
        assert_eq!(d.new_streak, 3);
        assert_eq!(d.new_next_log_at_streak, 4);

        // streak 3 -> 4: 4 >= 4, logs and doubles to 8.
        let prev = row(true, 3, 4);
        let d = decide(Some(&prev), true);
        assert!(d.should_log);
        assert_eq!(d.new_streak, 4);
        assert_eq!(d.new_next_log_at_streak, 8);
    }

    #[test]
    fn backoff_caps_and_keeps_logging_periodically() {
        let prev = row(false, 63, 64);
        let d = decide(Some(&prev), false);
        assert!(d.should_log);
        assert_eq!(d.new_streak, 64);
        assert_eq!(d.new_next_log_at_streak, 64);

        let prev = row(false, 127, 64);
        let d = decide(Some(&prev), false);
        assert!(d.should_log);
        assert_eq!(d.new_streak, 128);
        assert_eq!(d.new_next_log_at_streak, 64);
    }

    #[test]
    fn record_poll_outcome_notifies_once_on_first_failure_then_suppresses_mailbox() {
        let store = Store::open_in_memory().expect("open store");
        store
            .record_poll_outcome(
                PollerKind::BaseBranchFetch,
                "guardian-1",
                &PollOutcome::Unhealthy {
                    error: "auth failed".to_string(),
                },
            )
            .expect("record first failure");
        let page = store
            .mailbox_messages_for_client("test-client-ral-545-a", false, None)
            .expect("mailbox");
        assert_eq!(page.len(), 1, "exactly one notification on first failure");

        // A second, equivalent failure while already unhealthy must not
        // enqueue a second mailbox message.
        store
            .record_poll_outcome(
                PollerKind::BaseBranchFetch,
                "guardian-1",
                &PollOutcome::Unhealthy {
                    error: "auth failed".to_string(),
                },
            )
            .expect("record repeat failure");
        let page = store
            .mailbox_messages_for_client("test-client-ral-545-b", false, None)
            .expect("mailbox");
        assert_eq!(page.len(), 1, "no new notification on repeated failure");
    }

    #[test]
    fn record_poll_outcome_recovery_sends_no_mailbox_notification() {
        let store = Store::open_in_memory().expect("open store");
        store
            .record_poll_outcome(
                PollerKind::BaseBranchFetch,
                "guardian-2",
                &PollOutcome::Unhealthy {
                    error: "network unreachable".to_string(),
                },
            )
            .expect("record failure");
        store
            .record_poll_outcome(
                PollerKind::BaseBranchFetch,
                "guardian-2",
                &PollOutcome::Healthy,
            )
            .expect("record recovery");
        let page = store
            .mailbox_messages_for_client("test-client-ral-545-c", false, None)
            .expect("mailbox");
        assert_eq!(
            page.len(),
            1,
            "recovery itself must not add a second notification"
        );
    }

    #[test]
    fn clear_poller_health_for_guardian_only_clears_that_guardian() {
        let store = Store::open_in_memory().expect("open store");
        store
            .record_poll_outcome(
                PollerKind::BaseBranchFetch,
                "guardian-a",
                &PollOutcome::Healthy,
            )
            .expect("record a");
        store
            .record_poll_outcome(
                PollerKind::BaseBranchFetch,
                "guardian-b",
                &PollOutcome::Healthy,
            )
            .expect("record b");

        store
            .clear_poller_health_for_guardian("guardian-a")
            .expect("clear a");

        assert!(
            store
                .get_poller_health(PollerKind::BaseBranchFetch, "guardian-a")
                .expect("get a")
                .is_none()
        );
        assert!(
            store
                .get_poller_health(PollerKind::BaseBranchFetch, "guardian-b")
                .expect("get b")
                .is_some()
        );
    }

    #[test]
    fn clear_poller_health_resets_backoff_so_next_poll_logs_as_fresh() {
        let store = Store::open_in_memory().expect("open store");
        for _ in 0..5 {
            store
                .record_poll_outcome(PollerKind::PrForge, "guardian-c", &PollOutcome::Healthy)
                .expect("record healthy");
        }
        let before = store
            .get_poller_health(PollerKind::PrForge, "guardian-c")
            .expect("get")
            .expect("row exists");
        assert!(before.streak > 1);

        store
            .clear_poller_health_for_guardian("guardian-c")
            .expect("clear");

        assert!(
            store
                .get_poller_health(PollerKind::PrForge, "guardian-c")
                .expect("get after clear")
                .is_none()
        );
    }

    #[test]
    fn clear_all_poller_health_clears_every_review() {
        let store = Store::open_in_memory().expect("open store");
        store
            .record_poll_outcome(
                PollerKind::BaseBranchFetch,
                "guardian-x",
                &PollOutcome::Healthy,
            )
            .expect("record x");
        store
            .record_poll_outcome(PollerKind::PrForge, "guardian-y", &PollOutcome::Healthy)
            .expect("record y");

        store.clear_all_poller_health().expect("clear all");

        assert!(
            store
                .get_poller_health(PollerKind::BaseBranchFetch, "guardian-x")
                .expect("get x")
                .is_none()
        );
        assert!(
            store
                .get_poller_health(PollerKind::PrForge, "guardian-y")
                .expect("get y")
                .is_none()
        );
    }
}
