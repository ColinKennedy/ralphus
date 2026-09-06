//! The Arbiter (RAL-318): the daemon's single, singleton subsystem that
//! classifies a Triage-opted-in cell into a registered Triage type, and
//! backs the `ralphus check health` round-trip against its own configured
//! agent/model.
//!
//! Exactly one Arbiter exists per daemon, never per-project, with its own
//! `agent`/`model`/`maximum_budget_usd` config (`crate::config::ArbiterConfig`
//! / `[arbiter]`) -- wholly separate from any review's own conflict-resolver
//! `agent`/`model`. Both namespaces use plain `agent`/`model` field names (no
//! `resolver_`/`arbiter_` prefix): they never collide, so there is no real
//! need for a disambiguating prefix on either side (see `docs/glossary.md`).
//!
//! Classification is single-attempt, no retry (RAL-318): a timeout, provider
//! error, or an unparseable/unrecognized reply all permanently assign
//! [`UNCLASSIFIED_TYPE`] rather than retrying. This module owns that
//! decision; storage of the result lives in `crate::triage`.

use crate::chat_client::{self, ChatMessage};
use crate::config::ArbiterConfig;
use crate::store::Store;
use crate::triage::{TriageTypeView, UNCLASSIFIED_TYPE};

/// The resolved, daemon-singleton Arbiter -- computed fresh from
/// `crate::config::load_arbiter_config()` at each call site rather than held
/// as long-lived process state; see that function's doc comment for why this
/// is equivalent to a literal singleton in practice.
#[derive(Debug, Clone, PartialEq)]
pub struct Arbiter {
    pub agent: String,
    pub model: Option<String>,
    pub maximum_budget_usd: Option<f64>,
}

impl Arbiter {
    #[must_use]
    pub fn from_config(cfg: &ArbiterConfig) -> Self {
        Self {
            agent: cfg.agent().to_string(),
            model: cfg.model.clone(),
            maximum_budget_usd: cfg.maximum_budget_usd,
        }
    }

    /// The current, freshly-configured Arbiter -- the entry point every
    /// call site (submit-time classification, the health-check handler, the
    /// scheduler's Triage tick) should use.
    #[must_use]
    pub fn current() -> Self {
        Self::from_config(&crate::config::load_arbiter_config())
    }
}

/// Very approximate Anthropic per-model pricing, USD per million tokens, as
/// `(input, output)`. Used only to give the Arbiter's own budget cap a real
/// dollar figure to compare against for its tiny, infrequent classification/
/// health-check calls -- NOT a general-purpose pricing table (the runner's
/// own cell cost accounting comes from each backend's own self-reported
/// cost, which this bypasses entirely by calling the provider API directly).
/// An unrecognized model falls back to a conservative mid-range estimate
/// rather than `$0`, so an unbounded-spend scenario still eventually trips
/// the cap instead of silently reading as free forever.
fn anthropic_price_per_mtok(model: &str) -> (f64, f64) {
    let m = model.to_lowercase();
    if m.contains("haiku") {
        (0.80, 4.00)
    } else if m.contains("opus") {
        (15.00, 75.00)
    } else {
        // sonnet and anything unrecognized
        (3.00, 15.00)
    }
}

/// Estimate the USD cost of one call. Ollama is local inference with no API
/// cost, so this is always exactly `$0` for it -- not an approximation.
/// Anthropic/Claude uses [`anthropic_price_per_mtok`]. Any other backend
/// (which `chat_client::call_direct_with_usage` already refuses to call)
/// also resolves to `$0`.
fn estimate_cost_usd(agent: &str, model: &str, usage: chat_client::ChatUsage) -> f64 {
    match agent.to_lowercase().as_str() {
        "claude" | "anthropic" => {
            let (in_price, out_price) = anthropic_price_per_mtok(model);
            (usage.tokens_in as f64 / 1_000_000.0) * in_price
                + (usage.tokens_out as f64 / 1_000_000.0) * out_price
        }
        _ => 0.0,
    }
}

/// Whether the Arbiter's cumulative recorded spend has already reached (or
/// passed) its configured cap. `None` cap means unbounded. Checked before
/// every classification/health-check call so a call never proceeds once the
/// cap is known to be exceeded -- RAL-318's "no window of unbounded spend"
/// requirement.
#[must_use]
pub fn over_budget(store: &Store, arbiter: &Arbiter) -> bool {
    let Some(cap) = arbiter.maximum_budget_usd else {
        return false;
    };
    store.arbiter_cost_total().unwrap_or(0.0) >= cap
}

/// Build the classification system prompt listing every candidate type's
/// name/label/description. Exposed for tests; not meant for use outside this
/// module.
fn classification_system_prompt(candidates: &[&TriageTypeView]) -> String {
    let list = candidates
        .iter()
        .map(|t| format!("- {}: {} -- {}", t.name, t.label, t.description))
        .collect::<Vec<_>>()
        .join("\n");
    format!(
        "You are the Arbiter, a router that classifies a unit of work against the following \
         named types based on its content. A unit of work may genuinely match more than one \
         type at once (e.g. both \"bug\" and \"investigation\") -- name every type that applies, \
         at least one. Reply with ONLY a comma-separated list of the matching type name(s) and \
         nothing else -- no punctuation beyond the commas, no explanation, no surrounding \
         quotes.\n\n{list}"
    )
}

/// Parse the Arbiter's reply against the candidate type names: a
/// comma-separated list of names (each matched exact, case-insensitive,
/// after trimming whitespace/quotes/trailing punctuation). Unrecognized or
/// empty entries are dropped; duplicates are collapsed, first-seen order
/// kept. Empty when no entry in the reply matches any candidate.
fn parse_classification_reply<'a>(
    reply: &str,
    candidates: &[&'a TriageTypeView],
) -> Vec<&'a TriageTypeView> {
    let mut matched: Vec<&TriageTypeView> = Vec::new();
    for piece in reply.split(',') {
        let picked = piece
            .trim()
            .trim_matches(|c: char| c == '"' || c == '\'' || c == '.' || c.is_whitespace());
        if picked.is_empty() {
            continue;
        }
        if let Some(t) = candidates
            .iter()
            .find(|t| t.name.eq_ignore_ascii_case(picked))
        {
            if !matched.iter().any(|m| m.name == t.name) {
                matched.push(t);
            }
        }
    }
    matched
}

/// Classify `cell_context` (the cell's own prompt/description -- the primary
/// signal available at submit time) into one or more of the registered
/// Triage types -- a unit of work can genuinely match more than one (e.g.
/// both "bug" and "investigation"), so this returns every type the Arbiter
/// judges applicable, never just its top pick. Single attempt, no retry: any
/// failure (over budget, unsupported backend, provider error, a reply that
/// matches no registered type) permanently returns a single-element vec
/// holding [`UNCLASSIFIED_TYPE`].
/// Every outcome is logged via a Cartographer `Note` (see
/// `crate::cartographer`), linked to `squad_id`/`cell_id` -- mirroring
/// `crate::ark`'s `Note::new("ark")` call as the template for the `Note`
/// itself, but (unlike an Ark sweep, which isn't scoped to any one entity)
/// attaching the affected cell, since a classification always is.
#[must_use]
pub fn classify(
    store: &Store,
    arbiter: &Arbiter,
    squad_id: &str,
    cell_id: &str,
    cell_context: &str,
) -> Vec<String> {
    let types = store.list_triage_types().unwrap_or_default();
    let candidates: Vec<&TriageTypeView> = types
        .iter()
        .filter(|t| t.name != UNCLASSIFIED_TYPE)
        .collect();
    if candidates.is_empty() {
        return note_and_return(
            store,
            squad_id,
            cell_id,
            &[UNCLASSIFIED_TYPE.to_string()],
            "no registered triage types",
        );
    }
    if over_budget(store, arbiter) {
        return note_and_return(
            store,
            squad_id,
            cell_id,
            &[UNCLASSIFIED_TYPE.to_string()],
            "Arbiter maximum_budget_usd cap already reached; classification skipped",
        );
    }
    let system = classification_system_prompt(&candidates);
    let messages = [ChatMessage {
        role: "user",
        content: cell_context.to_string(),
        image: None,
    }];
    let (reply, usage) = match chat_client::call_direct_with_usage(
        &arbiter.agent,
        arbiter.model.as_deref(),
        &system,
        &messages,
    ) {
        Ok(v) => v,
        Err(e) => {
            return note_and_return(
                store,
                squad_id,
                cell_id,
                &[UNCLASSIFIED_TYPE.to_string()],
                &format!("classification call failed: {e}"),
            );
        }
    };
    let cost = estimate_cost_usd(
        &arbiter.agent,
        arbiter.model.as_deref().unwrap_or_default(),
        usage,
    );
    let _ = store.record_arbiter_cost(
        "classification",
        usage.tokens_in as i64,
        usage.tokens_out as i64,
        cost,
    );
    let matched = parse_classification_reply(&reply, &candidates);
    if matched.is_empty() {
        return note_and_return(
            store,
            squad_id,
            cell_id,
            &[UNCLASSIFIED_TYPE.to_string()],
            &format!("unrecognized classification reply {reply:?}"),
        );
    }
    let names: Vec<String> = matched.into_iter().map(|t| t.name.clone()).collect();
    let detail = format!("classified as [{}]", names.join(", "));
    note_and_return(store, squad_id, cell_id, &names, &detail)
}

fn note_and_return(
    store: &Store,
    squad_id: &str,
    cell_id: &str,
    result: &[String],
    detail: &str,
) -> Vec<String> {
    crate::cartographer::Note::new("arbiter")
        .squad(squad_id)
        .cell(cell_id)
        .emit(
            store,
            format!(
                "Arbiter classification -> [{}] ({detail})",
                result.join(", ")
            ),
            serde_json::json!({ "result": result, "detail": detail }),
        );
    result.to_vec()
}

/// The `ralphus check health` Arbiter round-trip (RAL-318): a live
/// completion call against the configured Arbiter agent/model, reporting
/// success or failure. User-triggered only (never polled), so this needs no
/// caching/rate-limiting of its own cost -- it still respects the Arbiter's
/// `maximum_budget_usd` cap like any other Arbiter call.
///
/// # Errors
/// Returns a human-readable failure reason: the cap already being reached,
/// an unsupported backend, a provider error, or an empty reply.
pub fn health_check(store: &Store, arbiter: &Arbiter) -> Result<String, String> {
    if over_budget(store, arbiter) {
        return Err(format!(
            "Arbiter maximum_budget_usd cap (${:.4}) already reached",
            arbiter.maximum_budget_usd.unwrap_or_default()
        ));
    }
    let messages = [ChatMessage {
        role: "user",
        content: "Reply with exactly one word: pong".to_string(),
        image: None,
    }];
    let (reply, usage) = chat_client::call_direct_with_usage(
        &arbiter.agent,
        arbiter.model.as_deref(),
        "You are a health-check echo service.",
        &messages,
    )?;
    let cost = estimate_cost_usd(
        &arbiter.agent,
        arbiter.model.as_deref().unwrap_or_default(),
        usage,
    );
    let _ = store.record_arbiter_cost(
        "health_check",
        usage.tokens_in as i64,
        usage.tokens_out as i64,
        cost,
    );
    if reply.trim().is_empty() {
        return Err("Arbiter agent returned an empty reply".to_string());
    }
    crate::cartographer::Note::new("arbiter").emit(
        store,
        format!(
            "Arbiter health check round-trip succeeded via {}",
            arbiter.agent
        ),
        serde_json::json!({ "agent": arbiter.agent, "model": arbiter.model }),
    );
    Ok(reply.trim().to_string())
}

// ── Store-side cost ledger ───────────────────────────────────────────────────

impl Store {
    /// Record one Arbiter call's cost as a line item (RAL-318). Mirrors
    /// `guardian_merge.rs`'s `record_guardian_call_cost` pattern, minus the
    /// guardian/branch keys the Arbiter has none of.
    ///
    /// # Errors
    /// Propagates any SQLite failure.
    pub fn record_arbiter_cost(
        &self,
        kind: &str,
        tokens_in: i64,
        tokens_out: i64,
        cost_usd: f64,
    ) -> crate::store::Result<()> {
        self.conn.execute(
            "INSERT INTO arbiter_costs(kind, tokens_in, tokens_out, cost_usd, created_at_ms)
             VALUES(?,?,?,?,?)",
            rusqlite::params![
                kind,
                tokens_in,
                tokens_out,
                cost_usd,
                crate::store::now_ms()
            ],
        )?;
        Ok(())
    }

    /// Cumulative Arbiter spend across every recorded call (classification
    /// and health-check calls alike -- RAL-318 tracks both against the same
    /// cap).
    ///
    /// # Errors
    /// Propagates any SQLite failure.
    pub fn arbiter_cost_total(&self) -> crate::store::Result<f64> {
        self.conn
            .query_row(
                "SELECT COALESCE(SUM(cost_usd), 0) FROM arbiter_costs",
                [],
                |r| r.get(0),
            )
            .map_err(Into::into)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::triage::TriageTypeView;

    fn store() -> Store {
        Store::open_in_memory().expect("in-memory store")
    }

    fn ty(name: &str, label: &str, description: &str) -> TriageTypeView {
        TriageTypeView {
            name: name.to_string(),
            label: label.to_string(),
            description: description.to_string(),
            created_at_ms: 0,
        }
    }

    #[test]
    fn arbiter_from_config_defaults_to_ollama() {
        let a = Arbiter::from_config(&ArbiterConfig::default());
        assert_eq!(a.agent, "ollama");
        assert_eq!(a.model, None);
        assert_eq!(a.maximum_budget_usd, None);
    }

    #[test]
    fn parse_classification_reply_matches_case_insensitively_and_trims() {
        let types = [ty("security", "Security", ""), ty("perf", "Perf", "")];
        let candidates: Vec<&TriageTypeView> = types.iter().collect();
        assert_eq!(
            parse_classification_reply("Security", &candidates)
                .iter()
                .map(|t| t.name.as_str())
                .collect::<Vec<_>>(),
            vec!["security"]
        );
        assert_eq!(
            parse_classification_reply("  \"PERF\".\n", &candidates)
                .iter()
                .map(|t| t.name.as_str())
                .collect::<Vec<_>>(),
            vec!["perf"]
        );
        assert!(parse_classification_reply("not-a-type", &candidates).is_empty());
    }

    #[test]
    fn parse_classification_reply_matches_multiple_comma_separated_types() {
        let types = [
            ty("bug", "Bug", ""),
            ty("investigation", "Investigation", ""),
            ty("feature", "Feature", ""),
        ];
        let candidates: Vec<&TriageTypeView> = types.iter().collect();
        assert_eq!(
            parse_classification_reply(" bug, investigation ", &candidates)
                .iter()
                .map(|t| t.name.as_str())
                .collect::<Vec<_>>(),
            vec!["bug", "investigation"]
        );
        // Unrecognized entries are dropped, duplicates collapsed.
        assert_eq!(
            parse_classification_reply("bug, nope, bug", &candidates)
                .iter()
                .map(|t| t.name.as_str())
                .collect::<Vec<_>>(),
            vec!["bug"]
        );
    }

    #[test]
    fn over_budget_is_false_with_no_cap_and_true_once_cap_reached() {
        let s = store();
        let arbiter = Arbiter {
            agent: "ollama".to_string(),
            model: None,
            maximum_budget_usd: None,
        };
        assert!(!over_budget(&s, &arbiter));
        let capped = Arbiter {
            maximum_budget_usd: Some(0.01),
            ..arbiter
        };
        assert!(!over_budget(&s, &capped));
        s.record_arbiter_cost("classification", 100, 50, 0.02)
            .unwrap();
        assert!(over_budget(&s, &capped));
    }

    #[test]
    fn arbiter_cost_total_accumulates_across_kinds() {
        let s = store();
        assert_eq!(s.arbiter_cost_total().unwrap(), 0.0);
        s.record_arbiter_cost("classification", 10, 10, 0.001)
            .unwrap();
        s.record_arbiter_cost("health_check", 5, 5, 0.0005).unwrap();
        assert!((s.arbiter_cost_total().unwrap() - 0.0015).abs() < 1e-9);
    }

    #[test]
    fn classify_falls_back_to_unclassified_with_no_registered_types() {
        let s = store();
        // A fresh store also seeds `DEFAULT_TRIAGE_TYPES` (RAL-318) alongside
        // the built-in `unclassified` type -- deregister those to exercise
        // the "no candidates at all" fallback this test targets.
        for (name, ..) in crate::triage::DEFAULT_TRIAGE_TYPES {
            s.deregister_triage_type(name).unwrap();
        }
        let arbiter = Arbiter {
            agent: "ollama".to_string(),
            model: None,
            maximum_budget_usd: None,
        };
        assert_eq!(
            classify(&s, &arbiter, "squad-1", "cell-1", "do some work"),
            vec![UNCLASSIFIED_TYPE.to_string()]
        );
    }

    #[test]
    fn classify_falls_back_to_unclassified_when_over_budget() {
        let s = store();
        s.register_triage_type("security", "Security", "sensitive changes")
            .unwrap();
        let arbiter = Arbiter {
            agent: "ollama".to_string(),
            model: None,
            maximum_budget_usd: Some(0.0),
        };
        s.record_arbiter_cost("classification", 1, 1, 0.0001)
            .unwrap();
        assert_eq!(
            classify(&s, &arbiter, "squad-1", "cell-1", "do some work"),
            vec![UNCLASSIFIED_TYPE.to_string()]
        );
    }

    #[test]
    fn classify_falls_back_to_unclassified_for_unsupported_backend() {
        let s = store();
        s.register_triage_type("security", "Security", "sensitive changes")
            .unwrap();
        let arbiter = Arbiter {
            agent: "claude-code".to_string(), // not headlessly callable
            model: None,
            maximum_budget_usd: None,
        };
        assert_eq!(
            classify(&s, &arbiter, "squad-1", "cell-1", "do some work"),
            vec![UNCLASSIFIED_TYPE.to_string()]
        );
    }

    #[test]
    fn health_check_reports_cap_reached_without_making_a_call() {
        let s = store();
        let arbiter = Arbiter {
            agent: "ollama".to_string(),
            model: None,
            maximum_budget_usd: Some(0.0),
        };
        s.record_arbiter_cost("health_check", 1, 1, 0.0001).unwrap();
        let err = health_check(&s, &arbiter).unwrap_err();
        assert!(err.contains("maximum_budget_usd"), "{err}");
    }

    #[test]
    fn health_check_fails_clearly_for_unsupported_backend() {
        let s = store();
        let arbiter = Arbiter {
            agent: "codex".to_string(),
            model: None,
            maximum_budget_usd: None,
        };
        let err = health_check(&s, &arbiter).unwrap_err();
        assert!(err.contains("not supported"), "{err}");
    }

    #[test]
    fn estimate_cost_is_zero_for_ollama() {
        let usage = chat_client::ChatUsage {
            tokens_in: 1_000_000,
            tokens_out: 1_000_000,
        };
        assert_eq!(estimate_cost_usd("ollama", "qwen3:8b", usage), 0.0);
    }

    #[test]
    fn estimate_cost_is_positive_for_claude() {
        let usage = chat_client::ChatUsage {
            tokens_in: 1_000_000,
            tokens_out: 1_000_000,
        };
        assert!(estimate_cost_usd("claude", "claude-haiku-4-5", usage) > 0.0);
    }
}
