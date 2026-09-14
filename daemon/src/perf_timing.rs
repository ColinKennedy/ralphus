//! Dormant-by-default phase timing for board-tab-relevant server endpoints
//! (RAL-414).
//!
//! Every board tab's cold navigation is expected to complete inside a
//! documented budget (see [`BOARD_COLD_LOAD_BUDGET_MS`]) measured end to end:
//! server work, the wire, and client render. Diagnosing a budget miss needs a
//! breakdown of where the time went, but that breakdown must cost nothing on
//! the hot path when nobody is looking. This module only measures anything
//! when [`TIMING_ENV_VAR`] is set to `1`/`true` in the environment (checked
//! once and cached, see [`timing_enabled`]) -- the same opt-in shape as
//! [`crate::otel`]'s `OTEL_EXPORTER_OTLP_ENDPOINT` gate. Disabled, a
//! [`PhaseTimer`] is a single cached-bool branch per checkpoint; nothing else
//! runs.
//!
//! Enabled, each instrumented handler's checkpoints become:
//!   * a `Server-Timing` response header (the standard `name;dur=ms` wire
//!     format -- both browser DevTools and
//!     `PerformanceResourceTiming.serverTiming` parse this natively, so the
//!     board's own opt-in client-side timing, see
//!     `librarian/assets/board/85-perf-timing.js`, reads server phases with
//!     no bespoke wire format of its own), and
//!   * one `[performance]`-tagged `rlog!` line, in the same "name=Xms" shape
//!     `/api/task-index` used ad hoc before this module existed.
//!
//! This module is compile-time inert: no `cfg(debug_assertions)`, no Cargo
//! profile dependency. It behaves identically in a debug build, a release
//! build, or a release build with debug symbols kept -- only the environment
//! variable, read at runtime, decides whether it does anything, so it works
//! the same way regardless of how the daemon binary was built.

use std::sync::OnceLock;
use std::time::Instant;

/// Set to `1` or `true` to turn on phase timing across every instrumented
/// endpoint (`Server-Timing` header + `[performance]` log line). Unset (the
/// default) costs one cached bool check per checkpoint and nothing else.
pub const TIMING_ENV_VAR: &str = "RALPHUS_BOARD_TIMING";

/// Documented default cold-load budget (RAL-414): every supported board
/// tab's true cold navigation -- clean session/browser state, first
/// navigation to that tab, measured from the start of setup-free work
/// through user-visible readiness -- must land inside this many
/// milliseconds. Do not hardcode this number anywhere else; call
/// [`board_cold_load_budget_ms`], which applies the [`BUDGET_ENV_VAR`]
/// override, instead.
pub const BOARD_COLD_LOAD_BUDGET_MS: u64 = 2000;

/// Overrides [`BOARD_COLD_LOAD_BUDGET_MS`] for slower CI machines or manual
/// diagnostics. The browser-side test suite
/// (`cli-py/tests/test_board_cold_load_perf.py`) reads this same variable
/// name, so both sides agree on one budget without duplicating the number.
pub const BUDGET_ENV_VAR: &str = "RALPHUS_BOARD_COLD_LOAD_BUDGET_MS";

/// Stable phase names shared by every instrumented endpoint, so a
/// `Server-Timing` breakdown means the same thing everywhere: time spent
/// waiting for the store mutex, time spent building the response's view
/// model once the store is held, and time spent serializing it to JSON.
/// Endpoint-specific work (e.g. retirement's live derivation cost) is folded
/// into [`PHASE_VIEW`] rather than inventing a new name per endpoint, so a
/// dashboard or regression test can compare `lock_wait`/`view`/`serialize`
/// across every board tab directly.
pub const PHASE_LOCK_WAIT: &str = "lock_wait";
pub const PHASE_VIEW: &str = "view";
pub const PHASE_SERIALIZE: &str = "serialize";

fn env_flag_enabled(var: &str) -> bool {
    std::env::var(var).is_ok_and(|v| v == "1" || v.eq_ignore_ascii_case("true"))
}

static TIMING_ENABLED: OnceLock<bool> = OnceLock::new();

/// Whether phase timing is turned on for this process. Reads
/// [`TIMING_ENV_VAR`] once via `OnceLock` and caches the result, so every
/// call after the first is a single load with no environment lookup.
pub fn timing_enabled() -> bool {
    *TIMING_ENABLED.get_or_init(|| env_flag_enabled(TIMING_ENV_VAR))
}

/// [`BOARD_COLD_LOAD_BUDGET_MS`], overridden by [`BUDGET_ENV_VAR`] when it is
/// set to a valid, non-zero integer. Not cached (unlike [`timing_enabled`]):
/// budget checks are far rarer than timing checkpoints, and tests that flip
/// this env var mid-process (see `daemon/tests/board_cold_load_perf.rs`)
/// need each call to see the current value.
pub fn board_cold_load_budget_ms() -> u64 {
    std::env::var(BUDGET_ENV_VAR)
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .filter(|ms| *ms > 0)
        .unwrap_or(BOARD_COLD_LOAD_BUDGET_MS)
}

/// A sequence of named checkpoints against one wall-clock start, for one
/// request. Cheap enough to construct unconditionally at the top of a
/// handler: [`PhaseTimer::start`] does one [`timing_enabled`] check, and if
/// disabled every subsequent [`PhaseTimer::phase`] call is just another
/// cached-bool branch that returns immediately.
pub struct PhaseTimer {
    enabled: bool,
    last: Instant,
    phases: Vec<(&'static str, u128)>,
}

impl PhaseTimer {
    /// Start timing. Call at the top of a handler, before its first
    /// checkpoint-worthy piece of work (typically acquiring the store lock).
    pub fn start() -> Self {
        PhaseTimer {
            enabled: timing_enabled(),
            last: Instant::now(),
            phases: Vec::new(),
        }
    }

    /// Record the elapsed time since the previous checkpoint (or [`start`])
    /// under `name`, then reset the checkpoint clock. A no-op (other than
    /// the enabled check) when timing is disabled.
    pub fn phase(&mut self, name: &'static str) {
        if !self.enabled {
            return;
        }
        let now = Instant::now();
        self.phases
            .push((name, now.duration_since(self.last).as_millis()));
        self.last = now;
    }

    /// Finish timing: emit the `[performance]` log line and build the
    /// `Server-Timing` header value. Returns `None` (and logs nothing) when
    /// timing is disabled or no phase was ever recorded. `endpoint` and
    /// `body_len` appear only in the log line -- the wire response's own
    /// path is already implicit in whichever endpoint called this, and
    /// `Server-Timing` carries no room for a byte count.
    pub fn finish(self, endpoint: &str, body_len: usize) -> Option<String> {
        if !self.enabled || self.phases.is_empty() {
            return None;
        }
        let header = self
            .phases
            .iter()
            .map(|(name, ms)| format!("{name};dur={ms}"))
            .collect::<Vec<_>>()
            .join(", ");
        let logged = self
            .phases
            .iter()
            .map(|(name, ms)| format!("{name}={ms}ms"))
            .collect::<Vec<_>>()
            .join(" ");
        // ralphus[ignore-rlog-pair]: per-request perf timing on a hot GET endpoint, opt-in only; a Cartographer row per request would flood the table
        crate::rlog!(
            INFO,
            "ralphus [performance] {endpoint} {logged} bytes={body_len}"
        );
        Some(header)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // These tests build `PhaseTimer`s directly with a fixed `enabled` value
    // rather than going through `timing_enabled()`/the env var: `nextest`
    // runs tests in parallel within one process, so mutating
    // `RALPHUS_BOARD_TIMING` here would race every other test that reads it,
    // and `timing_enabled()`'s `OnceLock` would cache whichever test won
    // that race for the rest of the process.

    #[test]
    fn disabled_timer_produces_no_header_or_phases() {
        let mut timer = PhaseTimer {
            enabled: false,
            last: Instant::now(),
            phases: Vec::new(),
        };
        timer.phase(PHASE_LOCK_WAIT);
        timer.phase(PHASE_VIEW);
        assert_eq!(timer.finish("test", 10), None);
    }

    #[test]
    fn enabled_timer_reports_every_phase_in_order() {
        let mut timer = PhaseTimer {
            enabled: true,
            last: Instant::now(),
            phases: Vec::new(),
        };
        timer.phase(PHASE_LOCK_WAIT);
        timer.phase(PHASE_VIEW);
        timer.phase(PHASE_SERIALIZE);
        let header = timer.finish("test-endpoint", 42).expect("timing enabled");
        let lock_wait_pos = header.find("lock_wait;dur=").expect("lock_wait present");
        let view_pos = header.find("view;dur=").expect("view present");
        let serialize_pos = header.find("serialize;dur=").expect("serialize present");
        assert!(lock_wait_pos < view_pos);
        assert!(view_pos < serialize_pos);
    }

    #[test]
    fn enabled_timer_with_no_phases_reports_nothing() {
        let timer = PhaseTimer {
            enabled: true,
            last: Instant::now(),
            phases: Vec::new(),
        };
        assert_eq!(timer.finish("test", 0), None);
    }

    #[test]
    fn env_flag_enabled_accepts_one_and_true_only() {
        assert!(!env_flag_enabled("RALPHUS_PERF_TIMING_TEST_UNSET_VAR"));
    }

    #[test]
    fn budget_default_matches_documented_two_seconds() {
        assert_eq!(BOARD_COLD_LOAD_BUDGET_MS, 2000);
    }

    #[test]
    fn budget_env_var_name_is_stable() {
        // Regression guard: this string is duplicated in
        // `cli-py/tests/test_board_cold_load_perf.py` and daemon/docs; a
        // rename here without updating those goes unnoticed otherwise.
        assert_eq!(BUDGET_ENV_VAR, "RALPHUS_BOARD_COLD_LOAD_BUDGET_MS");
    }
}
