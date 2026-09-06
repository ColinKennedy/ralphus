//! Typed classification of a remote-execution failure (RAL-355 Phase 0
//! remainder): distinguishes *why* a remote call failed so Cartographer and
//! a human reading it can tell "retry me" from "needs a human" without
//! parsing prose.
//!
//! **Deliberately scoped.** Classification is applied at the boundary where
//! a remote failure is about to be logged (`daemon/src/scheduler.rs`'s
//! "cell completed" Cartographer event), via heuristic pattern-matching on
//! the message text every remote call site already produces -- not by
//! threading a typed error through every `Result<_, String>` call site in
//! `remote_runner.rs`/`worktrees.rs`. That larger rewrite is what
//! `REMOTE_IMPROVEMENTS.local.md`'s Phase 0 scoping note sizes at
//! "comparable to Phase 1" (i.e. its own dedicated session) -- this gives
//! Cartographer/CLI/board a structural signal today without it.

/// Why a remote call failed, coarse enough to answer "should this be
/// retried automatically, or does it need a human" without a human reading
/// the message.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RemoteFailureKind {
    /// The machine (or its provider program) could not be reached at all --
    /// SSH connection refused/timed out/unresolvable, a dead provider
    /// program, a permission/host-key failure.
    Unreachable,
    /// A precondition the daemon checks before ever dispatching to the
    /// provider was unmet -- no registered clone URL, no configured
    /// `[machine.targets.*]` entry, an unsupported provider protocol version.
    PrerequisiteFailed,
    /// The provider reached the machine but could not prepare the
    /// workspace (clone/fetch/worktree-create failed, an origin-identity
    /// mismatch).
    ProvisionFailed,
    /// The provider reached the machine and the workspace existed, but the
    /// runner itself could not be started.
    LaunchFailed,
    /// A previously dispatched job's durable state could not be found or
    /// was corrupt on a later `status`/`stream`/`cancel` call -- the
    /// process may still be running unsupervised.
    Lost,
    /// The work was cancelled -- by a user, a cost cap, or daemon-restart
    /// reconciliation. Not a failure to retry; recorded distinctly so it
    /// never gets confused with one.
    Cancelled,
    /// The work exceeded its wall-clock timeout.
    TimedOut,
    /// The provider replied, but its response could not be parsed or
    /// violated the documented contract (no JSON, no `result`/`handle`,
    /// unparseable final line).
    InvalidResult,
    /// Recognized as a remote failure but not confidently classifiable
    /// further -- deliberately distinct from `Unreachable`'s "certainly a
    /// connectivity problem" so an unclear case isn't silently misfiled as
    /// one of the more specific, actionable kinds.
    Unknown,
}

impl RemoteFailureKind {
    /// Whether this kind is generally safe to retry automatically without
    /// operator action. Conservative: only kinds whose cause plausibly
    /// resolves on its own (a transient network blip, an unsupervised
    /// process that may have already finished, an unclassified failure
    /// worth one more attempt) are retryable; anything requiring a config
    /// or environment fix is not.
    #[must_use]
    pub fn is_retryable(self) -> bool {
        matches!(
            self,
            Self::Unreachable | Self::LaunchFailed | Self::Lost | Self::Unknown
        )
    }
}

impl std::fmt::Display for RemoteFailureKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = match self {
            Self::Unreachable => "unreachable",
            Self::PrerequisiteFailed => "prerequisite_failed",
            Self::ProvisionFailed => "provision_failed",
            Self::LaunchFailed => "launch_failed",
            Self::Lost => "lost",
            Self::Cancelled => "cancelled",
            Self::TimedOut => "timed_out",
            Self::InvalidResult => "invalid_result",
            Self::Unknown => "unknown",
        };
        f.write_str(s)
    }
}

/// Classify a remote failure message. Message-text heuristics, checked in
/// an order chosen so a more specific/actionable match wins over a vaguer
/// one when a message could plausibly hit more than one pattern.
#[must_use]
pub fn classify(message: &str) -> RemoteFailureKind {
    let lower = message.to_lowercase();
    if lower.contains("cancelled") || lower.contains("canceled") {
        RemoteFailureKind::Cancelled
    } else if lower.contains("could not run machine provider")
        || lower.contains("could not run ssh")
        || lower.contains("could not reach")
        || lower.contains("connection refused")
        || lower.contains("connection timed out")
        || lower.contains("could not resolve")
        || lower.contains("permission denied")
        || lower.contains("host key")
        || lower.contains("unreachable")
    {
        // Checked before the generic timeout pattern below: a *connection*
        // timing out is a reachability problem, not the work itself running
        // long, even though both messages happen to contain "timed out".
        RemoteFailureKind::Unreachable
    } else if lower.contains("timed out") || lower.contains("timeout") {
        RemoteFailureKind::TimedOut
    } else if lower.contains("no registered clone url")
        || lower.contains("has no configured target")
        || lower.contains("requires a configured")
        || lower.contains("declares contract version")
        || lower.contains("is not registered")
    {
        RemoteFailureKind::PrerequisiteFailed
    } else if lower.contains("job state") && (lower.contains("corrupt") || lower.contains("lost"))
        || lower.contains("could not find job")
        || lower == "lost"
    {
        RemoteFailureKind::Lost
    } else if lower.contains("provision") {
        RemoteFailureKind::ProvisionFailed
    } else if lower.contains("produced no json")
        || lower.contains("unparseable")
        || lower.contains("produced no output")
        || lower.contains("produced no result")
        || lower.contains("neither result nor handle")
    {
        RemoteFailureKind::InvalidResult
    } else if lower.contains("could not spawn") || lower.contains("could not launch") {
        RemoteFailureKind::LaunchFailed
    } else {
        RemoteFailureKind::Unknown
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unreachable_patterns_classify_as_unreachable() {
        for msg in [
            "could not run machine provider \"ssh\" (/opt/ralphus-ssh-provider): No such file",
            "connection refused",
            "connection timed out while establishing the ssh session",
            "permission denied (publickey)",
            "host key verification failed -- unknown host key",
        ] {
            assert_eq!(classify(msg), RemoteFailureKind::Unreachable, "{msg}");
        }
    }

    #[test]
    fn prerequisite_patterns_classify_as_prerequisite_failed() {
        for msg in [
            "project \"x\" has no registered clone URL",
            "machine \"ssh:devbox\" has no configured target",
            "asynchronous SSH execution requires a configured [machine.targets.*] entry",
            "machine provider \"ssh\" declares contract version 2, but this daemon implements 1",
        ] {
            assert_eq!(
                classify(msg),
                RemoteFailureKind::PrerequisiteFailed,
                "{msg}"
            );
        }
    }

    #[test]
    fn provision_failures_classify_distinctly() {
        assert_eq!(
            classify("provision failed: origin mismatch"),
            RemoteFailureKind::ProvisionFailed
        );
    }

    #[test]
    fn cancellation_is_never_confused_with_a_failure_to_retry() {
        assert_eq!(
            classify("cancelled before dispatch to the machine provider"),
            RemoteFailureKind::Cancelled
        );
        assert!(!RemoteFailureKind::Cancelled.is_retryable());
    }

    #[test]
    fn timeouts_classify_as_timed_out() {
        assert_eq!(
            classify("cell timed out after 3600 seconds"),
            RemoteFailureKind::TimedOut
        );
    }

    #[test]
    fn invalid_result_patterns_classify_distinctly() {
        for msg in [
            "machine provider \"ssh\" produced no JSON on stdout",
            "provider returned unparseable JSON: expected value at line 1",
            "an exec returning neither result nor handle is a contract violation",
        ] {
            assert_eq!(classify(msg), RemoteFailureKind::InvalidResult, "{msg}");
        }
    }

    #[test]
    fn an_unrecognized_message_is_unknown_not_misclassified() {
        assert_eq!(
            classify("something unexpected happened"),
            RemoteFailureKind::Unknown
        );
    }

    #[test]
    fn retryable_kinds_are_exactly_the_conservative_set() {
        assert!(RemoteFailureKind::Unreachable.is_retryable());
        assert!(RemoteFailureKind::LaunchFailed.is_retryable());
        assert!(RemoteFailureKind::Lost.is_retryable());
        assert!(RemoteFailureKind::Unknown.is_retryable());
        assert!(!RemoteFailureKind::PrerequisiteFailed.is_retryable());
        assert!(!RemoteFailureKind::ProvisionFailed.is_retryable());
        assert!(!RemoteFailureKind::Cancelled.is_retryable());
        assert!(!RemoteFailureKind::TimedOut.is_retryable());
        assert!(!RemoteFailureKind::InvalidResult.is_retryable());
    }

    #[test]
    fn display_matches_the_serde_rename() {
        assert_eq!(
            RemoteFailureKind::PrerequisiteFailed.to_string(),
            "prerequisite_failed"
        );
        assert_eq!(
            serde_json::to_string(&RemoteFailureKind::PrerequisiteFailed).unwrap(),
            "\"prerequisite_failed\""
        );
    }
}
