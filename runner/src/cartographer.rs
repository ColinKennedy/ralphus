//! Structured event emission over the `RALPHUS_EVENT:` stderr marker, ported
//! from `cli/src/ralphus/runner/cartographer.py`. The runner has no direct
//! database access (it's a subprocess, possibly on a remote machine via a
//! machine provider) and stdout is reserved for the `CellSpec`/
//! `CellResult` JSON contract, so structured events piggyback on stderr
//! behind this marker -- `daemon/src/runner.rs` already reads the child's
//! stderr line-by-line looking for exactly this prefix (`EVENT_MARKER`) and
//! forwards matches into Cartographer. Kept as its own stderr hop rather than
//! collapsed into a direct `Store` call, matching the existing architecture.

use serde_json::json;

/// Must match `daemon/src/runner.rs::EVENT_MARKER` exactly.
const EVENT_MARKER: &str = "RALPHUS_EVENT: ";

/// The message every agent backend uses for its per-turn token/cost snapshot
/// (RAL-161). It is a heartbeat, not a notable event: the daemon folds the
/// payload onto the cell row (`tokens_in`/`tokens_out`/`cost_usd`) and feeds
/// it to the live cost-cap check, then deliberately drops the event instead
/// of persisting a Cartographer row for it -- one row per assistant turn
/// buried a squad's timeline in noise. `daemon/src/runner.rs::LIVE_USAGE_MESSAGE`
/// keys that suppression on this exact string, so the two must stay in sync.
pub const LIVE_USAGE_MESSAGE: &str = "live usage";

#[derive(Debug, Clone, Copy, Default)]
pub struct EventContext<'a> {
    pub squad_id: Option<&'a str>,
    pub cell_id: Option<&'a str>,
    pub task: Option<&'a str>,
}

/// Emits one structured event line to stderr. `level` mirrors Python's
/// default of `"info"`.
pub fn emit(
    source: &str,
    message: &str,
    level: &str,
    ctx: EventContext<'_>,
    payload: serde_json::Value,
) {
    emit_scoped(source, message, level, None, ctx, payload);
}

/// As [`emit`], but also carries a `scope` tag (e.g. `"cell"`) --
/// `daemon/src/runner.rs::forward_runner_event` passes this straight through
/// onto the persisted `CartographerEntry`, the same free-text convention
/// `daemon/src/cartographer.rs::Note` uses for daemon-originated events.
pub fn emit_scoped(
    source: &str,
    message: &str,
    level: &str,
    scope: Option<&str>,
    ctx: EventContext<'_>,
    payload: serde_json::Value,
) {
    // RAL-380: live usage fires on nearly every streamed chunk (far more
    // often than compaction), so the raw `RALPHUS_EVENT:` line below is the
    // majority of what a human sees scrolling a claude-code/pi pane. Every
    // backend's payload shares this exact shape, so -- unlike `[compact]`,
    // which each backend prints for itself since its trigger differs per
    // backend -- this one friendly line covers all of them from here.
    if message == LIVE_USAGE_MESSAGE {
        eprintln!("{}", format_live_usage_line(&payload));
    }
    let mut body = json!({
        "source": source,
        "message": message,
        "level": level,
        "payload": payload,
    });
    if let Some(o) = body.as_object_mut() {
        if let Some(scope) = scope {
            o.insert("scope".to_string(), json!(scope));
        }
        if let Some(squad_id) = ctx.squad_id {
            o.insert("squad_id".to_string(), json!(squad_id));
        }
        if let Some(cell_id) = ctx.cell_id {
            o.insert("cell_id".to_string(), json!(cell_id));
        }
        if let Some(task) = ctx.task {
            o.insert("task".to_string(), json!(task));
        }
    }
    eprintln!("{EVENT_MARKER}{body}");
}

/// Renders a live-usage payload (`{tokens_in, tokens_out,
/// cache_creation_tokens, cache_read_tokens, cost_usd}`, the shape every
/// backend that emits [`LIVE_USAGE_MESSAGE`] uses) as one `[usage]` line,
/// matching the `[tool]`/`[compact]` convention instead of leaving it as the
/// raw JSON the line below still carries for the daemon to parse. Missing
/// fields default to `0`/`0.0` rather than erroring, since this is a
/// best-effort human aid, not the parse path anything depends on.
///
/// Deliberately uses `label: value` rather than `label=value` -- `tokens_in`/
/// `tokens_out` contain the substring `TOKEN`, which `ralphus_core::redact`'s
/// credential scrubber (RAL-247) treats as a secret env-var name in any
/// `KEY=value`-shaped text in a transcript, by design over-matching rather
/// than risk an unredacted API token. An `=` here would get the token
/// *counts* masked as `[REDACTED]` right alongside real secrets; `:` isn't a
/// shape that scrubber recognizes as an assignment at all.
fn format_live_usage_line(payload: &serde_json::Value) -> String {
    let tokens_in = payload["tokens_in"].as_i64().unwrap_or(0);
    let tokens_out = payload["tokens_out"].as_i64().unwrap_or(0);
    let cache_creation = payload["cache_creation_tokens"].as_i64().unwrap_or(0);
    let cache_read = payload["cache_read_tokens"].as_i64().unwrap_or(0);
    let cost_usd = payload["cost_usd"].as_f64().unwrap_or(0.0);
    format!(
        "[usage] tokens_in: {tokens_in}, tokens_out: {tokens_out}, \
         cache_creation: {cache_creation}, cache_read: {cache_read}, cost: ${cost_usd:.4}"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn marker_matches_daemon_constant() {
        assert_eq!(EVENT_MARKER, "RALPHUS_EVENT: ");
    }

    /// The daemon drops these rows by matching this literal, so a rename here
    /// alone would silently refill every squad timeline with per-turn usage.
    #[test]
    fn live_usage_message_matches_daemon_constant() {
        assert_eq!(LIVE_USAGE_MESSAGE, "live usage");
    }

    /// RAL-380: the exact payload shape claude-code/pi emit for live usage.
    #[test]
    fn format_live_usage_line_renders_every_field() {
        let line = format_live_usage_line(&json!({
            "tokens_in": 3094,
            "tokens_out": 3437,
            "cache_creation_tokens": 0,
            "cache_read_tokens": 277_504,
            "cost_usd": 0.0052598340000000006,
        }));
        assert_eq!(
            line,
            "[usage] tokens_in: 3094, tokens_out: 3437, cache_creation: 0, \
             cache_read: 277504, cost: $0.0053"
        );
    }

    /// A malformed/partial payload must render zeros, not panic -- this is a
    /// best-effort display aid, never the parse path anything depends on.
    #[test]
    fn format_live_usage_line_defaults_missing_fields_to_zero() {
        assert_eq!(
            format_live_usage_line(&json!({})),
            "[usage] tokens_in: 0, tokens_out: 0, cache_creation: 0, cache_read: 0, cost: $0.0000"
        );
    }

    /// RAL-380 regression: an earlier `tokens_in=NNNN` shape got its own
    /// token *counts* masked by `ralphus_core::redact::redact_secrets`,
    /// because `tokens_in`/`tokens_out` contain the substring `TOKEN` and the
    /// `=` made it look like a credential assignment. The `:` form must
    /// survive that scrubber completely untouched.
    #[test]
    fn format_live_usage_line_survives_the_credential_scrubber() {
        let line = format_live_usage_line(&json!({
            "tokens_in": 12,
            "tokens_out": 34,
            "cache_creation_tokens": 473,
            "cache_read_tokens": 30_903,
            "cost_usd": 0.0314,
        }));
        assert_eq!(
            ralphus_core::redact::redact_secrets(&line),
            std::borrow::Cow::Borrowed(line.as_str()),
            "the live-usage line must not be mistaken for a credential assignment"
        );
    }
}
