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
//!
//! RAL-346 adds a second, independent Arbiter responsibility:
//! [`infer_subprojects`] matches a cell's description against its project's
//! configured monorepo subprojects (`crate::config::MonorepoConfig`, seeded
//! only when the cell declared no manual `CellDef.subprojects` of its own),
//! so Triage pools can key by `(project, subproject)` rather than just
//! `(project)`. It shares this module's classification posture (single
//! attempt, no retry) and the same `[arbiter] maximum_budget_usd` cap, but
//! unlike classification's `UNCLASSIFIED_TYPE` fallback, a failed/no-match
//! inference simply leaves the cell [`crate::triage::SubprojectResolution::
//! Unresolved`] -- see that type's doc comment for the full three-state
//! model.
//!
//! RAL-412 adds a third, independent Arbiter responsibility:
//! [`order_pooled_candidates`] proposes a semantic order for the cells a
//! drained `(project, triage_type)` pool will build one automatic review
//! from. `crate::reviews::build_review_from_drained_pool` calls it exactly
//! once per review (the shared tail of the threshold-drain and cron-drain
//! paths), with one bounded, labeled aggregate request covering every
//! candidate. The reply is accepted only when it is an exact permutation of
//! the pool's stable candidate ids; any failure -- budget cap, transport
//! error, malformed/incomplete/duplicate/unknown ids -- leaves the caller's
//! deterministic pool order untouched. It shares the same
//! `[arbiter] maximum_budget_usd` cap as `classify` and `infer_subprojects`.

#[cfg(test)]
use std::sync::Arc;

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
///
/// Takes the shared store handle rather than an already-held `&Store`, and
/// locks it only for the brief reads/writes around the real work -- the
/// live, sometimes multi-second `chat_client` call runs with the lock
/// dropped, mirroring `guardian_merge::resolver_backend`'s
/// lock-read-drop-then-call shape. Holding the daemon's single store mutex
/// across that network call would stall every other store user (the
/// scheduler, every other API request) for its whole duration, not just the
/// caller.
#[must_use]
pub fn classify(
    store: &crate::store_lock::StoreHandle,
    arbiter: &Arbiter,
    squad_id: &str,
    cell_id: &str,
    cell_context: &str,
) -> Vec<String> {
    let guard = store.lock();
    let types = guard.list_triage_types().unwrap_or_default();
    let candidates: Vec<&TriageTypeView> = types
        .iter()
        .filter(|t| t.name != UNCLASSIFIED_TYPE)
        .collect();
    if candidates.is_empty() {
        return note_and_return(
            &guard,
            squad_id,
            cell_id,
            &[UNCLASSIFIED_TYPE.to_string()],
            "no registered triage types",
        );
    }
    if over_budget(&guard, arbiter) {
        return note_and_return(
            &guard,
            squad_id,
            cell_id,
            &[UNCLASSIFIED_TYPE.to_string()],
            "Arbiter maximum_budget_usd cap already reached; classification skipped",
        );
    }
    let system = classification_system_prompt(&candidates);
    // `candidates` borrows `types`; both are local to this locked scope, so
    // clone the (tiny) view list out before dropping the guard.
    let candidate_views: Vec<TriageTypeView> = candidates.into_iter().cloned().collect();
    drop(guard);

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
            let guard = store.lock();
            return note_and_return(
                &guard,
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
    let guard = store.lock();
    let _ = guard.record_arbiter_cost(
        "classification",
        usage.tokens_in as i64,
        usage.tokens_out as i64,
        cost,
    );
    let candidate_refs: Vec<&TriageTypeView> = candidate_views.iter().collect();
    let matched = parse_classification_reply(&reply, &candidate_refs);
    if matched.is_empty() {
        return note_and_return(
            &guard,
            squad_id,
            cell_id,
            &[UNCLASSIFIED_TYPE.to_string()],
            &format!("unrecognized classification reply {reply:?}"),
        );
    }
    let names: Vec<String> = matched.into_iter().map(|t| t.name.clone()).collect();
    let detail = format!("classified as [{}]", names.join(", "));
    note_and_return(&guard, squad_id, cell_id, &names, &detail)
}

/// Build the subproject-inference system prompt listing every candidate
/// subproject identifier (RAL-346). Mirrors [`classification_system_prompt`]'s
/// shape; a subproject has no label/description the way a `TriageTypeView`
/// does (`crate::config::MonorepoConfig` is a plain name list), so the
/// candidate list is just the bare names.
fn subproject_inference_system_prompt(candidates: &[String]) -> String {
    let list = candidates
        .iter()
        .map(|s| format!("- {s}"))
        .collect::<Vec<_>>()
        .join("\n");
    format!(
        "You are the Arbiter, a router that matches a unit of work's description against the \
         following known subprojects of a monorepo, based on its content. A unit of work may \
         genuinely touch more than one subproject at once -- name every one that applies. If \
         none of the subprojects plausibly apply, reply with exactly the single word NONE. \
         Otherwise reply with ONLY a comma-separated list of the matching subproject name(s) and \
         nothing else -- no punctuation beyond the commas, no explanation, no surrounding \
         quotes.\n\n{list}"
    )
}

/// Parse the Arbiter's subproject-inference reply against the candidate
/// names: a comma-separated list, each matched exact (case-insensitive,
/// after trimming whitespace/quotes/trailing punctuation) -- the same
/// matching rules as [`parse_classification_reply`], generalized to plain
/// `&str` candidates since a subproject carries no label/description.
/// Unrecognized/empty entries (including the literal `NONE` sentinel) are
/// dropped; duplicates collapsed, first-seen order kept.
fn parse_subproject_reply(reply: &str, candidates: &[String]) -> Vec<String> {
    let mut matched: Vec<String> = Vec::new();
    for piece in reply.split(',') {
        let picked = piece
            .trim()
            .trim_matches(|c: char| c == '"' || c == '\'' || c == '.' || c.is_whitespace());
        if picked.is_empty() || picked.eq_ignore_ascii_case("none") {
            continue;
        }
        if let Some(c) = candidates.iter().find(|c| c.eq_ignore_ascii_case(picked)) {
            if !matched.iter().any(|m| m == c) {
                matched.push(c.clone());
            }
        }
    }
    matched
}

/// The Arbiter's async subproject-inference step (RAL-346): match
/// `cell_context` (the cell's own prompt/description) against
/// `candidates` (a project's configured `[monorepo] subprojects` list) and
/// return every one the Arbiter judges applicable. Single attempt, no
/// retry -- mirroring [`classify`]'s posture -- but unlike `classify` there
/// is no `UNCLASSIFIED_TYPE`-style permanent fallback sentinel: any failure
/// (over budget, unsupported backend, provider error, no match found)
/// simply returns `None`, leaving the cell's resolution
/// [`crate::triage::SubprojectResolution::Unresolved`] rather than writing
/// anything -- per this ticket's binding decision to default to "leave
/// Unresolved" rather than invent a value. Shares the same
/// `[arbiter] maximum_budget_usd` cap as `classify` (RAL-346: "the same
/// subsystem doing more work, not a separate budget line").
///
/// Locks `store` only for the brief reads/writes around the real work,
/// exactly like `classify` -- the live LLM call itself runs with the lock
/// dropped.
#[must_use]
pub fn infer_subprojects(
    store: &crate::store_lock::StoreHandle,
    arbiter: &Arbiter,
    squad_id: &str,
    cell_id: &str,
    cell_context: &str,
    candidates: &[String],
) -> Option<Vec<String>> {
    if candidates.is_empty() || cell_context.trim().is_empty() {
        return None;
    }
    {
        let guard = store.lock();
        if over_budget(&guard, arbiter) {
            crate::cartographer::Note::new("arbiter")
                .squad(squad_id)
                .cell(cell_id)
                .emit(
                    &guard,
                    "Arbiter subproject inference skipped: maximum_budget_usd cap already reached",
                    serde_json::json!({}),
                );
            return None;
        }
    }
    let system = subproject_inference_system_prompt(candidates);
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
            let guard = store.lock();
            crate::cartographer::Note::new("arbiter")
                .squad(squad_id)
                .cell(cell_id)
                .emit(
                    &guard,
                    format!("Arbiter subproject inference call failed: {e}"),
                    serde_json::json!({}),
                );
            return None;
        }
    };
    let cost = estimate_cost_usd(
        &arbiter.agent,
        arbiter.model.as_deref().unwrap_or_default(),
        usage,
    );
    let guard = store.lock();
    let _ = guard.record_arbiter_cost(
        "subproject_inference",
        usage.tokens_in as i64,
        usage.tokens_out as i64,
        cost,
    );
    let matched = parse_subproject_reply(&reply, candidates);
    if matched.is_empty() {
        crate::cartographer::Note::new("arbiter")
            .squad(squad_id)
            .cell(cell_id)
            .emit(
                &guard,
                format!(
                    "Arbiter found no subproject match in reply {reply:?}; cell stays unresolved"
                ),
                serde_json::json!({}),
            );
        return None;
    }
    crate::cartographer::Note::new("arbiter")
        .squad(squad_id)
        .cell(cell_id)
        .emit(
            &guard,
            format!("Arbiter inferred subproject(s) [{}]", matched.join(", ")),
            serde_json::json!({ "subprojects": matched }),
        );
    Some(matched)
}

/// One Triage-opted-in cell that had no inline `triage_type` at submit time,
/// so it still needs an Arbiter classification call -- collected by `submit`
/// during its synchronous validation pass, classified later by
/// [`spawn_triage_followup`].
pub struct PendingClassification {
    pub task_idx: i64,
    pub idx: i64,
    pub cell_id: String,
    pub context: String,
}

/// One Triage-opted-in cell that declared no manual `CellDef.subprojects` at
/// submit time, so it still needs the Arbiter's async subproject-inference
/// step (RAL-346) -- collected by `submit` during its synchronous validation
/// pass (mirroring [`PendingClassification`]), resolved later by
/// [`spawn_triage_followup`]. A cell whose `CellDef.subprojects` *was*
/// non-empty never becomes one of these -- `submit` seeds its resolution
/// directly (`Store::set_cell_subprojects`, `inferred = false`), no LLM call
/// needed.
pub struct PendingSubprojectResolution {
    pub task_idx: i64,
    pub idx: i64,
    pub cell_id: String,
    /// The cell's declared `cwd`, used to locate its project's
    /// `.ralphus.toml` (`crate::config::load_monorepo_config`). `None`, or a
    /// `ralphus:`-prefixed worktree placeholder not yet materialized into a
    /// real path, both leave this cell `Unresolved` -- resolving a
    /// placeholder here would duplicate `derive_triage_pools`' own worktree
    /// materialization pass just to find out whether the project is even a
    /// monorepo, so this simpler, synchronous-friendly signal is used
    /// instead; a cell whose only `cwd` is a placeholder simply stays
    /// `Unresolved` and its keying falls back to the plain project key,
    /// same as any other not-yet-resolved monorepo cell.
    pub cwd: Option<String>,
    pub context: String,
}

/// Classifies every `pending` cell, resolves every `pending_subprojects`
/// cell's RAL-346 subproject state, then (whenever `has_triage` -- i.e. this
/// submission has at least one Triage-opted-in cell at all, typed or not)
/// runs [`crate::reviews::derive_triage_pools`] for `squad_id`, all on one
/// background thread. Spawned from `submit` right after its HTTP response is
/// built, so neither the classification/inference calls (one real, sometimes
/// multi-second LLM round-trip each) nor `derive_triage_pools` itself (which
/// resolves each cell's worktree placeholder -- a real, sometimes
/// multi-second `git worktree add` against a large repo, see
/// `worktrees::resolve_placeholders`'s doc comment) ever hold up that
/// response. This used to run `derive_triage_pools` synchronously in
/// `submit` for any already-typed (inline `triage_type`) cell, which made
/// the Simple tab's "Auto Review" default -- `triage = true`, no inline
/// type, so pooling still ran synchronously even before classification was
/// deferred here -- pay that same worktree-creation cost on every single
/// submission; moving it here as well fixes that for every case, not just
/// the classification one.
///
/// The subproject-inference pass runs strictly before `derive_triage_pools`
/// so its result (if any) is already persisted (`Store::set_cell_subprojects`)
/// by the time pool keys are computed -- but a cell for which it finds no
/// match, or can't even attempt (no real `cwd` yet, see
/// [`PendingSubprojectResolution::cwd`]'s doc comment), simply stays
/// `Unresolved` and `derive_triage_pools` falls back to the plain
/// project-name key for it, exactly as if this step hadn't run at all
/// (RAL-346: "a cell must not be blocked from being picked up for work if
/// the Arbiter step hasn't finished yet").
///
/// Mirrors `crate::generation`'s jobs never blocking on `POST
/// /api/generate`, and the store-handle-in-a-thread shape `crate::pr::
/// start_resync_pr_bases` uses. `derive_triage_pools` already tolerates a
/// cell with no resolved type yet (see its doc comment) by skipping it, so
/// leaving these cells un-pooled at submit time and re-running pooling here
/// once they're classified is safe. A no-op when `pending`,
/// `pending_subprojects`, and `has_triage` all call for nothing.
pub fn spawn_triage_followup(
    store_handle: crate::store_lock::StoreHandle,
    squad_id: String,
    file: ralphus_core::schema::TaskFile,
    pending: Vec<PendingClassification>,
    pending_subprojects: Vec<PendingSubprojectResolution>,
    has_triage: bool,
) {
    if pending.is_empty() && pending_subprojects.is_empty() && !has_triage {
        return;
    }
    std::thread::spawn(move || {
        let arbiter = Arbiter::current();
        for p in &pending {
            let types = classify(&store_handle, &arbiter, &squad_id, &p.cell_id, &p.context);
            let guard = store_handle.lock();
            let _ = guard.set_cell_triage_types(&squad_id, p.task_idx, p.idx, &types);
        }
        for p in &pending_subprojects {
            resolve_pending_subprojects(&store_handle, &arbiter, &squad_id, p);
        }
        let guard = store_handle.lock();
        if let Err(e) = crate::reviews::derive_triage_pools(&guard, &squad_id, &file, |cands| {
            crate::arbiter::order_pooled_candidates(
                &guard,
                &crate::arbiter::Arbiter::current(),
                cands,
            )
        }) {
            crate::rlog!(
                WARNING,
                "ralphus [arbiter] background triage pooling for squad {squad_id} failed: {}",
                e.message
            );
            crate::cartographer::Note::new("arbiter")
                .squad(&squad_id)
                .emit(
                    &guard,
                    format!("background triage pooling failed: {}", e.message),
                    serde_json::json!({ "error": e.message }),
                );
        }
    });
}

/// One `pending_subprojects` cell's worth of [`spawn_triage_followup`]'s
/// work (RAL-346): decide whether its project is even a monorepo, and if
/// so, run [`infer_subprojects`] and persist a match. A no-op (leaving the
/// cell `Unresolved`) when `cwd` is absent, is an unmaterialized
/// `ralphus:` worktree placeholder, or the project's `.ralphus.toml`
/// configures no `[monorepo] subprojects` at all.
fn resolve_pending_subprojects(
    store_handle: &crate::store_lock::StoreHandle,
    arbiter: &Arbiter,
    squad_id: &str,
    p: &PendingSubprojectResolution,
) {
    let Some(cwd) = p.cwd.as_deref() else {
        return;
    };
    if cwd.starts_with("ralphus:") {
        return;
    }
    let monorepo = crate::config::load_monorepo_config(std::path::Path::new(cwd));
    if !monorepo.is_monorepo() {
        return;
    }
    let Some(matched) = infer_subprojects(
        store_handle,
        arbiter,
        squad_id,
        &p.cell_id,
        &p.context,
        &monorepo.subprojects,
    ) else {
        return;
    };
    let guard = store_handle.lock();
    let _ = guard.set_cell_subprojects(squad_id, p.task_idx, p.idx, &matched, true);
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

// ── RAL-412: semantic ordering of a drained pool ─────────────────────────────

/// Max characters of one candidate's prompt/command *excerpt* carried into a
/// pool-ordering request ([`build_ordering_user_message`]). A cell's prompt
/// states its intent up front, so truncation prefers that concise leading
/// summary text over the tail (see [`ordering_excerpt`]), which is also what
/// makes the truncation useful: the first lines carry the signal.
pub const ORDERING_EXCERPT_CHARS: usize = 600;

/// Hard cap on the combined candidate content of one pool-ordering request
/// (RAL-412: "the combined arbiter input is capped to a reasonable
/// configured or documented limit"). Deliberately a documented fixed
/// constant rather than schema/config: per-candidate budget is
/// `TOTAL / n` bounded by [`ORDERING_EXCERPT_CHARS`], so the aggregate
/// content stays under this cap for every pool up to
/// `TOTAL / ORDERING_EXCERPT_FLOOR_CHARS` candidates. Beyond that (a
/// pathological pool), the per-candidate floor below wins and the aggregate
/// grows at `FLOOR * n` -- every candidate still contributes context, at
/// the cost of the headroom cap.
pub const ORDERING_TOTAL_CONTENT_CHARS: usize = 12_000;

/// Per-candidate excerpt floor once a pool is large enough that
/// `TOTAL / n` would shrink below it: every candidate — even in a huge
/// straggler sweep — still contributes at least this much prompt context to
/// the ordering request.
pub const ORDERING_EXCERPT_FLOOR_CHARS: usize = 64;

/// One drained pool candidate's labeled context for the RAL-412 ordering
/// request. The Arbiter is never shown raw pool/row identity (squad ids and
/// branch names are review bookkeeping, not intent) — it sees only the
/// stable [`ordering_candidate_id`] plus a bounded excerpt of the cell's
/// prompt/command context, which is exactly the signal a reviewer needs to
/// group related work.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OrderingCandidate {
    /// The stable candidate identifier the Arbiter is asked to echo back
    /// (see [`ordering_candidate_id`]).
    pub id: String,
    /// The cell's own `sid`, for Cartographer notes.
    pub cell_id: String,
    /// The cell's prompt/command text; may be empty when the row is
    /// unreadable, in which case the candidate participates in the ordering
    /// by id only.
    pub context: String,
}

/// The stable, human-addressable candidate id an ordering reply must name —
/// `squad_id/t{task_idx}:c{idx}`. Unambiguous across the whole pool (unlike
/// a cell's `sid`, which can repeat between squads) and printable, so the
/// Arbiter can echo it verbatim and the reply can be validated as an exact
/// permutation of the drained pool.
#[must_use]
pub fn ordering_candidate_id(squad_id: &str, task_idx: i64, idx: i64) -> String {
    format!("{squad_id}/t{task_idx}:c{idx}")
}

/// The per-candidate prompt-excerpt budget for an ordering request covering
/// `candidate_count` candidates: equal shares of
/// [`ORDERING_TOTAL_CONTENT_CHARS`], clamped into
/// `[ORDERING_EXCERPT_FLOOR_CHARS, ORDERING_EXCERPT_CHARS]`.
#[must_use]
pub fn ordering_content_budget(candidate_count: usize) -> usize {
    if candidate_count == 0 {
        return 0;
    }
    (ORDERING_TOTAL_CONTENT_CHARS / candidate_count)
        .clamp(ORDERING_EXCERPT_FLOOR_CHARS, ORDERING_EXCERPT_CHARS)
}

/// The bounded leading excerpt of one candidate's context — the first
/// `budget` characters (prompts summarize their intent up front), with a
/// trailing ellipsis when truncated, and surrounding whitespace trimmed.
#[must_use]
pub fn ordering_excerpt(context: &str, budget: usize) -> String {
    let trimmed = context.trim();
    if trimmed.chars().count() <= budget {
        return trimmed.to_string();
    }
    let mut excerpt: String = trimmed.chars().take(budget).collect::<String>();
    excerpt.push_str(" …[truncated]");
    excerpt
}

/// The RAL-412 ordering system prompt: static across requests, only the
/// labeled candidate list varies.
fn ordering_system_prompt() -> String {
    "You are the Arbiter, sequencing a set of candidate units of work into one \
     review's order. Order the candidates so that closely related work sits \
     adjacent: changes to the same area, feature, subsystem, or theme cluster \
     together. The order becomes the review's branch stack, so prefer a \
     coherent story over a strict priority sort."
        .to_string()
}

/// Build the Arbiter's user message for one pool-ordering request: every
/// candidate's bounded, labeled excerpt in one aggregate message (RAL-412:
/// one request for the whole pool, never one query per candidate or
/// pairwise comparisons). Pairing each labeled excerpt with a strict
/// reply-format instruction keeps the response parseable as a permutation.
#[must_use]
pub fn build_ordering_user_message(candidates: &[OrderingCandidate]) -> String {
    let budget = ordering_content_budget(candidates.len());
    let list = candidates
        .iter()
        .map(|c| format!("- {}: {}", c.id, ordering_excerpt(&c.context, budget)))
        .collect::<Vec<_>>()
        .join("\n");
    format!(
        "These are the pooled candidate units of work for one automatic review, each \
         labeled with its stable <id>. Examine each candidate's prompt/command context \
         and propose ONE review order that places closely related work together.\n\n\
         {list}\n\nReply with ONLY a comma-separated list of the <id>s in your proposed \
         order, every id exactly once, and nothing else -- no explanations, no \
         numbering, no surrounding quotes or punctuation beyond the commas."
    )
}

/// Parse the Arbiter's proposed ordering reply against the pool's stable
/// candidate ids. The reply is accepted only when it names **every**
/// expected id **exactly once**, in reply order — a strict permutation of
/// the drained pool — the contract `crate::reviews` relies on to reorder
/// review branches without omitting or duplicating a candidate (RAL-412:
/// "accept the response only if it is an exact permutation of the drained
/// pool"). Empty pieces are skipped (trailing commas, blank lines); every
/// non-empty piece must be a not-yet-placed expected id — an unknown id,
/// a duplicate, or any stray prose rejects the whole reply. `None` means
/// "not a valid permutation" and the caller falls back to the deterministic
/// pool order.
#[must_use]
pub fn parse_ordering_reply(reply: &str, expected_ids: &[&str]) -> Option<Vec<String>> {
    let mut remaining: std::collections::HashSet<String> =
        expected_ids.iter().map(|id| id.to_string()).collect();
    let mut ordered: Vec<String> = Vec::with_capacity(expected_ids.len());
    // Tolerate newlines/semicolons as separators alongside commas, and
    // surrounding quotes/brackets/punctuation, without relaxing the
    // exact-permutation check.
    let normalized = reply.replace("\n", ",").replace(";", ",");
    for piece in normalized.split(',') {
        let picked = piece.trim().trim_matches(|c: char| {
            c == '"'
                || c == '\''
                || c == '.'
                || c == '('
                || c == ')'
                || c == '['
                || c == ']'
                || c.is_whitespace()
        });
        if picked.is_empty() {
            continue;
        }
        let found: Option<String> = remaining.iter().find(|id| id.as_str() == picked).cloned();
        let id = found?;
        remaining.remove(&id);
        ordered.push(id);
    }
    remaining.is_empty().then_some(ordered)
}

/// Ask the configured Arbiter to propose a semantic review order for a
/// drained Triage pool (RAL-412): one bounded aggregate request carrying
/// every candidate's labeled prompt excerpt, answered with a proposed
/// order over the pool's stable candidate ids.
///
/// Returns `None` — meaning "keep the caller's deterministic pool order" —
/// on every failure path, exactly like `classify`'s `UNCLASSIFIED_TYPE`
/// fallback: a one/zero-candidate pool (trivially correct order, no call),
/// the `[arbiter] maximum_budget_usd` cap already being reached, an
/// unsupported backend or transport/provida error, or a reply that is not
/// an exact permutation of the pool (malformed, incomplete, duplicated, or
/// unknown ids). Every outcome is logged as a Cartographer `Note` (see
/// `crate::cartographer`).
///
/// The live call runs while the caller's store lock is held — the shared
/// tail of the drain paths (`crate::reviews::build_review_from_drained_pool`)
/// is always invoked from a context that already holds the daemon's single
/// `Store` mutex across the whole set-and-drain-create sequence, and the
/// request itself is a single bounded round-trip, on the same footing as
/// `health_check`'s user-triggered lock-held call (unlike the hot per-cell
/// `classify` path, which deliberately drops the lock around its call).
#[must_use]
pub fn order_pooled_candidates(
    store: &Store,
    arbiter: &Arbiter,
    candidates: &[OrderingCandidate],
) -> Option<Vec<String>> {
    let ids: Vec<String> = candidates.iter().map(|c| c.id.clone()).collect();
    // A one-candidate pool has exactly one valid order; don't spend an
    // Arbiter round-trip (or a cent of its budget) on it.
    if candidates.len() <= 1 {
        return Some(ids);
    }
    if over_budget(store, arbiter) {
        crate::cartographer::Note::new("arbiter").emit(
            store,
            "Arbiter pool ordering skipped: maximum_budget_usd cap already reached; \
             review uses the deterministic pool order",
            serde_json::json!({ "candidates": ids, "reason": "over_budget" }),
        );
        return None;
    }
    let system = ordering_system_prompt();
    let user = build_ordering_user_message(candidates);
    let messages = [ChatMessage {
        role: "user",
        content: user,
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
            crate::cartographer::Note::new("arbiter").emit(
                store,
                format!(
                    "Arbiter pool ordering call failed: {e}; review uses the deterministic pool order"
                ),
                serde_json::json!({
                    "candidates": ids,
                    "reason": format!("call_failed: {e}"),
                }),
            );
            return None;
        }
    };
    let cost = estimate_cost_usd(
        &arbiter.agent,
        arbiter.model.as_deref().unwrap_or_default(),
        usage,
    );
    let _ = store.record_arbiter_cost(
        "pool_ordering",
        usage.tokens_in as i64,
        usage.tokens_out as i64,
        cost,
    );
    let expected: Vec<&str> = ids.iter().map(|id| id.as_str()).collect();
    let Some(ordered) = parse_ordering_reply(&reply, &expected) else {
        crate::cartographer::Note::new("arbiter").emit(
            store,
            format!(
                "Arbiter pool ordering reply {reply:?} is not an exact permutation of the \
                 drained pool; review uses the deterministic pool order"
            ),
            serde_json::json!({
                "candidates": ids,
                "reason": "invalid_permutation",
                "reply": reply,
            }),
        );
        return None;
    };
    crate::cartographer::Note::new("arbiter").emit(
        store,
        format!(
            "Arbiter proposed semantic review order [{}]",
            ordered.join(", ")
        ),
        serde_json::json!({ "candidates": ids, "order": ordered }),
    );
    Some(ordered)
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
        let s = Arc::new(crate::store_lock::StoreMutex::new(store()));
        // A fresh store also seeds `DEFAULT_TRIAGE_TYPES` (RAL-318) alongside
        // the built-in `unclassified` type -- deregister those to exercise
        // the "no candidates at all" fallback this test targets.
        for (name, ..) in crate::triage::DEFAULT_TRIAGE_TYPES {
            s.lock().deregister_triage_type(name).unwrap();
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
        let s = Arc::new(crate::store_lock::StoreMutex::new(store()));
        s.lock()
            .register_triage_type("security", "Security", "sensitive changes")
            .unwrap();
        let arbiter = Arbiter {
            agent: "ollama".to_string(),
            model: None,
            maximum_budget_usd: Some(0.0),
        };
        s.lock()
            .record_arbiter_cost("classification", 1, 1, 0.0001)
            .unwrap();
        assert_eq!(
            classify(&s, &arbiter, "squad-1", "cell-1", "do some work"),
            vec![UNCLASSIFIED_TYPE.to_string()]
        );
    }

    #[test]
    fn classify_falls_back_to_unclassified_for_unsupported_backend() {
        let s = Arc::new(crate::store_lock::StoreMutex::new(store()));
        s.lock()
            .register_triage_type("security", "Security", "sensitive changes")
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

    // ── RAL-346: subproject inference ───────────────────────────────────────

    #[test]
    fn parse_subproject_reply_matches_case_insensitively_and_trims() {
        let candidates = vec!["core".to_string(), "utils".to_string()];
        assert_eq!(
            parse_subproject_reply("Core", &candidates),
            vec!["core".to_string()]
        );
        assert_eq!(
            parse_subproject_reply("  \"UTILS\".\n", &candidates),
            vec!["utils".to_string()]
        );
        assert!(parse_subproject_reply("not-a-subproject", &candidates).is_empty());
    }

    #[test]
    fn parse_subproject_reply_matches_multiple_and_drops_the_none_sentinel() {
        let candidates = vec!["core".to_string(), "utils".to_string(), "steam".to_string()];
        assert_eq!(
            parse_subproject_reply(" core, utils ", &candidates),
            vec!["core".to_string(), "utils".to_string()]
        );
        assert!(parse_subproject_reply("NONE", &candidates).is_empty());
        // Unrecognized entries dropped, duplicates collapsed.
        assert_eq!(
            parse_subproject_reply("core, nope, core", &candidates),
            vec!["core".to_string()]
        );
    }

    #[test]
    fn infer_subprojects_returns_none_with_no_candidates_or_empty_context() {
        let s = Arc::new(crate::store_lock::StoreMutex::new(store()));
        let arbiter = Arbiter {
            agent: "ollama".to_string(),
            model: None,
            maximum_budget_usd: None,
        };
        assert!(
            infer_subprojects(&s, &arbiter, "squad-1", "cell-1", "do some work", &[]).is_none()
        );
        assert!(
            infer_subprojects(
                &s,
                &arbiter,
                "squad-1",
                "cell-1",
                "   ",
                &["core".to_string()]
            )
            .is_none()
        );
    }

    #[test]
    fn infer_subprojects_returns_none_when_over_budget() {
        let s = Arc::new(crate::store_lock::StoreMutex::new(store()));
        let arbiter = Arbiter {
            agent: "ollama".to_string(),
            model: None,
            maximum_budget_usd: Some(0.0),
        };
        s.lock()
            .record_arbiter_cost("classification", 1, 1, 0.0001)
            .unwrap();
        assert!(
            infer_subprojects(
                &s,
                &arbiter,
                "squad-1",
                "cell-1",
                "fix the core module",
                &["core".to_string()]
            )
            .is_none(),
            "over the shared Arbiter budget cap must skip the call, not spend further"
        );
    }

    #[test]
    fn infer_subprojects_returns_none_for_unsupported_backend() {
        let s = Arc::new(crate::store_lock::StoreMutex::new(store()));
        let arbiter = Arbiter {
            agent: "claude-code".to_string(), // not headlessly callable
            model: None,
            maximum_budget_usd: None,
        };
        assert!(
            infer_subprojects(
                &s,
                &arbiter,
                "squad-1",
                "cell-1",
                "fix the core module",
                &["core".to_string()]
            )
            .is_none()
        );
    }

    // ── RAL-412: semantic ordering of a drained pool ────────────────────────

    fn candidate(id: &str, context: &str) -> OrderingCandidate {
        OrderingCandidate {
            id: id.to_string(),
            cell_id: format!("cell-of-{id}"),
            context: context.to_string(),
        }
    }

    #[test]
    fn ordering_content_budget_scales_with_pool_size_and_stays_capped() {
        assert_eq!(ordering_content_budget(0), 0);
        // Small pools get each candidate's full excerpt.
        assert_eq!(ordering_content_budget(3), ORDERING_EXCERPT_CHARS);
        // A mid-size pool shares the aggregate cap evenly.
        let mid = ordering_content_budget(40);
        assert!(mid < ORDERING_EXCERPT_CHARS);
        assert_eq!(mid, ORDERING_TOTAL_CONTENT_CHARS / 40);
        // A pathological pool still leaves every candidate a floor.
        assert_eq!(ordering_content_budget(250), ORDERING_EXCERPT_FLOOR_CHARS);
        // The aggregate stays within the documented cap while the floor doesn't
        // preempt it.
        for n in 1..=(ORDERING_TOTAL_CONTENT_CHARS / ORDERING_EXCERPT_FLOOR_CHARS) {
            assert!(
                ordering_content_budget(n) * n <= ORDERING_TOTAL_CONTENT_CHARS,
                "budget({n}) * {n} exceeds the aggregate cap"
            );
        }
    }

    #[test]
    fn ordering_excerpt_truncates_to_leading_chars_with_an_ellipsis() {
        let short = "fix the core module";
        assert_eq!(ordering_excerpt(short, 600), short);
        assert_eq!(ordering_excerpt("  \n{}", 5), "{}", "whitespace trimmed");
        let long = "a".repeat(1_000);
        let excerpt = ordering_excerpt(&long, 600);
        assert_eq!(
            excerpt.chars().count(),
            600 + " …[truncated]".chars().count()
        );
        assert!(
            excerpt.starts_with("aaaa"),
            "keeps the leading summary text"
        );
        assert!(excerpt.ends_with("…[truncated]"));
    }

    #[test]
    fn build_ordering_user_message_labels_every_candidate_with_a_bounded_excerpt() {
        let cands = vec![
            candidate("squad-1/t0:c0", &"x".repeat(9_999)),
            candidate("squad-1/t1:c2", "short prompt"),
        ];
        let msg = build_ordering_user_message(&cands);
        assert!(
            msg.contains("squad-1/t0:c0"),
            "labels the truncated candidate"
        );
        assert!(msg.contains("squad-1/t1:c2"));
        assert!(msg.contains("short prompt"));
        assert!(msg.contains("…[truncated]"));
        assert!(
            msg.contains("exactly once"),
            "instructs a strict permutation"
        );
    }

    #[test]
    fn parse_ordering_reply_accepts_an_exact_permutation() {
        let ids = ["squad-1/t0:c0", "squad-1/t1:c2", "squad-2/t0:c0"];
        // Comma-separated, in a genuinely different order.
        assert_eq!(
            parse_ordering_reply("squad-2/t0:c0, squad-1/t0:c0, squad-1/t1:c2", &ids),
            Some(vec![
                "squad-2/t0:c0".to_string(),
                "squad-1/t0:c0".to_string(),
                "squad-1/t1:c2".to_string(),
            ])
        );
        // Tolerates newlines, quotes, and trailing punctuation between items.
        assert_eq!(
            parse_ordering_reply("\"squad-1/t1:c2\"\nsquad-1/t0:c0; squad-2/t0:c0.", &ids),
            Some(vec![
                "squad-1/t1:c2".to_string(),
                "squad-1/t0:c0".to_string(),
                "squad-2/t0:c0".to_string(),
            ])
        );
        // The pool's own order is a valid permutation too.
        assert!(parse_ordering_reply(&ids.join(", "), &ids).is_some());
    }

    #[test]
    fn parse_ordering_reply_rejects_missing_duplicate_unknown_and_malformed() {
        let ids = ["squad-1/t0:c0", "squad-1/t1:c2", "squad-2/t0:c0"];
        // Incomplete: one id never named.
        assert!(parse_ordering_reply("squad-1/t0:c0, squad-1/t1:c2", &ids).is_none());
        // Duplicate: one id placed twice, another never.
        assert!(
            parse_ordering_reply(
                "squad-1/t0:c0, squad-1/t0:c0, squad-1/t1:c2, squad-2/t0:c0",
                &ids
            )
            .is_none()
        );
        // Unknown id mixed in, even once.
        assert!(
            parse_ordering_reply(
                "squad-1/t0:c0, made-up-id, squad-1/t1:c2, squad-2/t0:c0",
                &ids
            )
            .is_none()
        );
        // Stray prose is not an exact permutation either.
        assert!(
            parse_ordering_reply(
                "Here is my order: squad-1/t0:c0, squad-1/t1:c2, squad-2/t0:c0",
                &ids
            )
            .is_none()
        );
        // Empty / whitespace-only reply.
        assert!(parse_ordering_reply("", &ids).is_none());
        assert!(parse_ordering_reply("\n  \n", &ids).is_none());
        // Case matters: a respelled id is an unknown id.
        assert!(
            parse_ordering_reply("Squad-1/T0:C0, squad-1/t1:c2, squad-2/t0:c0", &ids).is_none()
        );
    }

    #[test]
    fn order_pooled_candidates_trivially_orders_a_single_candidate_without_any_call() {
        let s = store();
        // An unsupported backend proves no call is attempted: had the
        // function tried to reach the Arbiter it would fail and return None.
        let arbiter = Arbiter {
            agent: "codex".to_string(), // not headlessly callable
            model: None,
            maximum_budget_usd: None,
        };
        let cands = vec![candidate("squad-1/t0:c0", "do work")];
        assert_eq!(
            order_pooled_candidates(&s, &arbiter, &cands),
            Some(vec!["squad-1/t0:c0".to_string()])
        );
    }

    #[test]
    fn order_pooled_candidates_returns_none_when_over_budget() {
        let s = store();
        let arbiter = Arbiter {
            agent: "ollama".to_string(),
            model: None,
            maximum_budget_usd: Some(0.0),
        };
        s.record_arbiter_cost("classification", 1, 1, 0.0001)
            .unwrap();
        let cands = vec![
            candidate("squad-1/t0:c0", "a"),
            candidate("squad-1/t1:c2", "b"),
        ];
        assert!(
            order_pooled_candidates(&s, &arbiter, &cands).is_none(),
            "over the shared Arbiter budget cap must skip the call, not spend further"
        );
    }

    #[test]
    fn order_pooled_candidates_returns_none_for_unsupported_backend() {
        let s = store();
        let arbiter = Arbiter {
            agent: "claude-code".to_string(), // not headlessly callable
            model: None,
            maximum_budget_usd: None,
        };
        let cands = vec![
            candidate("squad-1/t0:c0", "a"),
            candidate("squad-1/t1:c2", "b"),
        ];
        assert!(
            order_pooled_candidates(&s, &arbiter, &cands).is_none(),
            "a failed Arbiter call must fall back, never reorder"
        );
    }
}
