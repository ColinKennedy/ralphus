//! Secret-value redaction (RAL-264).
//!
//! A resolved secret (an agent-profile `from_env` value, see
//! `crate::agent_profiles`) must never land in anything durable or
//! user-facing — not the daemon's SQLite store, the CLI's JSON output, or the
//! board UI. One leak path was a proof step's tmux pane text being folded
//! verbatim into a failure `detail` (and pane snapshot / terminal-log files)
//! while the pane still showed the agent's own `$env:... = 'sk-or-v1-...'`
//! assignment; the resolved token value was not scrubbed anywhere in that
//! path.
//!
//! This module owns the scrubbing. It is deliberately **precise-value**
//! redaction: the daemon registers the exact resolved values of every
//! `from_env`-sourced agent-profile env var, and each durable-pane sink
//! (`crate::tmux::write_pane_snapshot`, `crate::terminal_log::write_attempt`,
//! `crate::runner::read_tmux_result`) replaces those exact strings with
//! [`REDACTED`] before the text is persisted or surfaced. There is no
//! regex/pattern fallback — matching on the resolved value is what makes this
//! deterministic and safe from false positives.
//!
//! The registry is process-wide and meant to be seeded at daemon startup (and
//! whenever an agent profile is resolved for a cell); [`SecretValues`] is the
//! pure, unit-testable core the registry wraps.

use std::collections::BTreeSet;
use std::sync::{Mutex, OnceLock};

/// Literal replacement written in place of any registered secret value.
pub const REDACTED: &str = "<redacted>";

/// The process-wide set of resolved secret values to scrub.
///
/// Seeded by [`register_all`]/[`register`] — at daemon startup from every
/// agent profile the daemon can see, and again whenever a profile is resolved
/// for a cell — and consulted by [`redact_all`], which the durable-pane sinks
/// call before persisting/surfacing pane text.
static REGISTRY: OnceLock<Mutex<BTreeSet<String>>> = OnceLock::new();

fn registry() -> &'static Mutex<BTreeSet<String>> {
    REGISTRY.get_or_init(|| Mutex::new(BTreeSet::new()))
}

/// Register one resolved secret value to be scrubbed from durable/user-facing
/// text. Idempotent; empty values are ignored (an empty needle would match
/// everywhere).
pub fn register(value: &str) {
    if value.is_empty() {
        return;
    }
    if let Ok(mut guard) = registry().lock() {
        guard.insert(value.to_string());
    }
}

/// Clear every registered secret value. Test-only: wipes the process-wide
/// registry so a redaction test can set up its own secrets without leaking
/// them across tests in the same binary (the daemon test binaries run many
/// tests in parallel threads over one shared process).
#[cfg(test)]
pub fn clear_for_tests() {
    let Ok(mut guard) = registry().lock() else {
        return;
    };
    guard.clear();
}

/// Serialize registry-mutating tests. Test-only: several RAL-264 regression
/// tests (`*_redacts_registered_secret_values`) each `clear_for_tests()` then
/// `register(...)` their own needle against the single process-wide registry,
/// and they run on parallel threads. Without a shared lock one test's
/// `clear_for_tests()` can wipe a sibling's needle mid-flight, making that
/// sibling's `redact_all` a silent no-op and the assertion fail purely from
/// scheduling. Wrapping each such test body in this helper makes them
/// mutually exclusive, so the registry is always in a consistent state while
/// one is running.
#[cfg(test)]
pub fn with_registry_lock<T>(f: impl FnOnce() -> T) -> T {
    static TEST_LOCK: Mutex<()> = Mutex::new(());
    let guard = TEST_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let result = f();
    drop(guard);
    result
}

/// Register many resolved secret values at once (e.g. every `from_env` value
/// across an agent-profile map).
pub fn register_all(values: impl IntoIterator<Item = String>) {
    let Ok(mut guard) = registry().lock() else {
        return;
    };
    guard.extend(values.into_iter().filter(|v| !v.is_empty()));
}

/// Pure replacement core: replace every non-empty value in `secrets` with
/// [`REDACTED`] wherever it occurs in `text`.
///
/// Splitting out the pure form (from the process-wide [`redact_all`]) keeps
/// the replacement semantics unit-testable without depending on global state.
#[must_use]
pub fn redact(text: &str, secrets: &BTreeSet<String>) -> String {
    let mut out = text.to_string();
    for secret in secrets {
        if !secret.is_empty() {
            out = out.replace(secret.as_str(), REDACTED);
        }
    }
    out
}

/// Redact `text` against the process-wide registry of resolved secret values.
/// A no-op (returning `text` unchanged) when nothing has been registered.
#[must_use]
pub fn redact_all(text: &str) -> String {
    let Ok(guard) = registry().lock() else {
        return text.to_string();
    };
    redact(text, &guard)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redact_replaces_exact_matches_with_placeholder() {
        let secrets = BTreeSet::from(["sk-or-v1-secret".to_string()]);
        let out = redact("token=sk-or-v1-secret here", &secrets);
        assert_eq!(out, "token=<redacted> here");
    }

    #[test]
    fn redact_is_a_noop_when_nothing_matches() {
        let secrets = BTreeSet::from(["sk-or-v1-secret".to_string()]);
        let out = redact("no secrets in this line", &secrets);
        assert_eq!(out, "no secrets in this line");
    }

    #[test]
    fn redact_ignores_empty_secrets() {
        let secrets = BTreeSet::from([String::new(), "real".to_string()]);
        // An empty needle would match between every character; it must be
        // skipped so redaction never corrupts arbitrary text.
        assert_eq!(redact("abc", &secrets), "abc");
        assert_eq!(redact("real", &secrets), "<redacted>");
    }

    #[test]
    fn redact_replaces_every_occurrence_of_each_secret() {
        let secrets = BTreeSet::from(["secret".to_string(), "token".to_string()]);
        let out = redact("secret token secret", &secrets);
        assert_eq!(out, "<redacted> <redacted> <redacted>");
    }

    #[test]
    fn scrub_uses_the_placeholder_constant() {
        let secrets = BTreeSet::from(["value".to_string()]);
        assert_eq!(redact("value", &secrets), REDACTED);
    }
}
