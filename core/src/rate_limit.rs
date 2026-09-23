//! RAL-435: shared ceiling on how long ralphus will ever wait out a
//! provider-reported rate-limit delay ("retry after N seconds" / "try again
//! in N seconds") before retrying anyway. A provider's own reported delay is
//! untrusted input -- parsed out of free-form error text from an external
//! API and carried across a daemon/subprocess boundary -- so it is never
//! honored unbounded: a misconfigured, buggy, or hostile upstream that
//! reports an absurd delay (hours, days) must never be able to stall a
//! cell/proof or a review-worktree merge attempt indefinitely. Applied both
//! where the delay is parsed (`runner::pi_backend`) and where the daemon
//! ingests any backend's report of one (`daemon::runner::RunnerResult::rate_limited`),
//! so neither side of that boundary has to trust the other got it right.

/// The hard ceiling, in seconds: no provider-reported rate-limit delay is
/// ever waited out for longer than this, no matter what the message says.
pub const MAX_RATE_LIMIT_RETRY_SECS: u64 = 600; // 10 minutes

/// Clamps a provider-reported delay (in seconds) to [`MAX_RATE_LIMIT_RETRY_SECS`].
#[must_use]
pub fn clamp_retry_after_secs(secs: u64) -> u64 {
    secs.min(MAX_RATE_LIMIT_RETRY_SECS)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clamp_leaves_a_short_delay_untouched() {
        assert_eq!(clamp_retry_after_secs(5), 5);
    }

    #[test]
    fn clamp_leaves_exactly_the_ceiling_untouched() {
        assert_eq!(clamp_retry_after_secs(MAX_RATE_LIMIT_RETRY_SECS), 600);
    }

    #[test]
    fn clamp_caps_a_delay_over_ten_minutes() {
        assert_eq!(clamp_retry_after_secs(3600), MAX_RATE_LIMIT_RETRY_SECS);
    }

    #[test]
    fn clamp_caps_an_extreme_delay() {
        assert_eq!(clamp_retry_after_secs(u64::MAX), MAX_RATE_LIMIT_RETRY_SECS);
    }
}
