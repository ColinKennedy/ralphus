//! The `ModelBackend` trait, ported from `cli/src/ralphus/runner/backend.py`'s
//! `Protocol`. Every agent implementation (`claude-code`, `codex`, the
//! hand-rolled Anthropic/Ollama tool-loop, a generic external harness) is one
//! of these.

use crate::tools::Workspace;

/// A backend call failed. Mirrors Python's `BackendError`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BackendError(pub String);

impl std::fmt::Display for BackendError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl std::error::Error for BackendError {}

/// What a backend call produced. Mirrors Python's `BackendOutcome`.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct BackendOutcome {
    pub summary: String,
    pub tokens_in: i64,
    pub tokens_out: i64,
    /// RAL-326: prompt-cache *write* tokens -- input the provider billed at
    /// the cache-creation rate. Tracked separately from `tokens_in` (which
    /// keeps meaning "uncached input") because for an agentic coding session
    /// the cache columns are frequently the majority of billed input, and
    /// folding them in would silently change what every existing
    /// `tokens_in` reading means. `0` for a backend whose harness reports no
    /// cache breakdown at all (the native Anthropic/Ollama tool loop, an
    /// external harness).
    pub cache_creation_tokens: i64,
    /// RAL-326: prompt-cache *read* tokens -- input served from an existing
    /// cache entry at the discounted rate. See `cache_creation_tokens`.
    pub cache_read_tokens: i64,
    pub cost_usd: f64,
    pub agent_session_id: Option<String>,
    /// RAL-292: set when this turn ended with a backgrounded job the agent
    /// launched but never checked the result of (e.g. a `Bash` tool call
    /// with `run_in_background: true` with no later `BashOutput`/`KillShell`
    /// call before the turn's terminal result). Carries a short description
    /// of the abandoned job for the eventual error message. `None` for a
    /// normal turn, and always `None` for backends that don't track
    /// individual tool calls at all.
    pub abandoned_background_job: Option<String>,
    /// RAL-339: set the moment this run's compaction cadence crosses into
    /// thrash (see `crate::thrash::ThrashTracker::record_compaction`).
    /// When `Some`, the backend has already killed its child process early
    /// (at the compaction boundary, not waiting for the run to finish) and
    /// every other field carries only whatever was captured live up to that
    /// point. `execute.rs::run_with_backend` checks this before anything
    /// else and fails the cell outright.
    pub compaction_thrash: Option<crate::thrash::ThrashDetail>,
}

/// Options common to every backend's `run` call, bundled to keep the trait's
/// signature from growing a new positional parameter every time a backend
/// needs one more piece of context.
#[derive(Debug, Clone, Default)]
pub struct RunOptions<'a> {
    pub model: Option<&'a str>,
    pub append_system_prompt: Option<&'a str>,
    pub resume_agent_session_id: Option<&'a str>,
    /// A session id the daemon pre-generated before this cell started
    /// (RAL-288 Stage 1). The claude-code backend passes it as
    /// `--session-id` when not resuming; other backends ignore it.
    pub assigned_agent_session_id: Option<&'a str>,
    pub timeout_sec: Option<u64>,
    /// RAL-304: resolved context-window token limit, or `None` for no cap.
    /// Only ever `Some` for a backend whose
    /// [`ModelBackend::supports_maximum_context`] returns `true` -- `core`'s
    /// `agent_supports_maximum_context` validation gate rejects it for
    /// anything else before submission.
    pub maximum_context: Option<u64>,
    /// RAL-304: resolved auto-compact trigger threshold in tokens, or `None`
    /// for no explicit threshold. Only ever `Some` for a backend whose
    /// [`ModelBackend::supports_auto_compact_threshold`] returns `true` --
    /// `core`'s `agent_supports_auto_compact_threshold` validation gate
    /// rejects it for anything else before submission. Accepted by a wider
    /// set of backends than [`Self::maximum_context`] (e.g. claude-code
    /// accepts this but not that).
    pub auto_compact_threshold: Option<u64>,
    /// RAL-303: how many characters of a `tool_use` argument value to render
    /// into the Live View tmux pane before truncating, resolved daemon-side
    /// from `[live_view] tool_arg_truncate_chars`. Support is optional per
    /// backend -- only the claude-code backend reads this today; others
    /// ignore it, mirroring `assigned_agent_session_id`'s "hand-rolled
    /// backends accept and ignore it" precedent. `None` means the reading
    /// backend should fall back to its own default.
    pub tool_arg_truncate_chars: Option<u32>,
    /// RAL-339: resolved `.ralphus.toml` `[thrash]` thresholds -- N (compact
    /// count) and M (turn-gap), daemon-resolved and forwarded per cell/proof
    /// the same way [`Self::tool_arg_truncate_chars`] is. `None` for either
    /// field falls back to `crate::thrash`'s own default (mirrors that
    /// field's "unset means backend default" precedent).
    pub thrash_max_compactions: Option<u32>,
    pub thrash_min_turn_gap: Option<u32>,
    /// RAL-333: resolved cap on how many tokens a single tool-call output
    /// (e.g. a large file read) may inject into the agent's context, or
    /// `None` for no cap. Only ever `Some` for a backend whose
    /// [`ModelBackend::supports_maximum_tool_output_tokens`] returns `true` --
    /// `core`'s `agent_supports_maximum_tool_output_tokens` validation gate
    /// rejects it for anything else before submission. Ralphus never
    /// truncates the output itself -- it only configures the backend's own
    /// native mechanism (an env var, a CLI arg, or an on-disk settings file --
    /// see `ClaudeCodeBackend`/`CodexBackend`/`PiBackend`'s own overrides for
    /// which) and defers entirely to that backend's behavior once the cap is
    /// set.
    pub maximum_tool_output_tokens: Option<u64>,
    /// RAL-336: whether this session may load the operator's personal
    /// settings/config. Defaults to `false` (isolated) via `RunOptions`'s
    /// `Default` derive. Only the claude-code/codex/pi backends read this;
    /// others ignore it, same "hand-rolled backends accept and ignore it"
    /// precedent as `assigned_agent_session_id`.
    pub allow_personal_settings: bool,
    /// RAL-336: whether this session may load the operator's personal
    /// cross-project memory. Defaults to `false` (isolated). Codex and Pi
    /// only expose a single config-directory lever that can't cleanly
    /// separate settings from memory -- those backends isolate whenever
    /// either this or [`Self::allow_personal_settings`] is `false`.
    pub allow_personal_memory: bool,
}

/// RAL-292: the sole turn content sent by [`ModelBackend::nudge`]'s default
/// caller-facing convention -- deliberately generic (not the cell's own task
/// text, which the resumed session already has), same principle as
/// `daemon/src/scheduler.rs::RESUME_AUTOMATION_PROMPT` (RAL-288).
pub const BACKGROUND_JOB_NUDGE_PROMPT: &str = "You ended your last turn with an unresolved backgrounded job -- \
    something you launched as a background/async process without ever \
    checking its actual result. Check that job's real result now (e.g. the \
    `BashOutput` tool, for a `Bash` call made with `run_in_background` set), \
    and finish the work you were originally asked to do based on what it \
    actually produced. Do not launch another background job to check this \
    one.";

pub trait ModelBackend {
    fn run(
        &self,
        prompt: &str,
        workspace: &Workspace,
        options: &RunOptions<'_>,
    ) -> Result<BackendOutcome, BackendError>;

    /// RAL-292: give a session that ended its turn with an unresolved
    /// backgrounded job a chance to check that job's real result and finish
    /// the work, instead of the cell being silently marked done or (worse)
    /// left as an abandoned background process. The caller sets
    /// `options.resume_agent_session_id` to the session being nudged before
    /// calling this.
    ///
    /// Returns `Ok(None)` when this backend has no live mid-run injection
    /// support -- the caller must then treat the abandoned job as
    /// unrecoverable rather than retry indefinitely. The default
    /// implementation is exactly this no-op, so a backend only needs to
    /// override it if it can meaningfully resume a session at all.
    fn nudge(
        &self,
        workspace: &Workspace,
        options: &RunOptions<'_>,
    ) -> Result<Option<BackendOutcome>, BackendError> {
        let _ = (workspace, options);
        Ok(None)
    }

    /// RAL-304: whether this backend has a real delivery mechanism for
    /// `RunOptions::maximum_context` -- a way to actually cap the
    /// context-window ceiling itself (an env var, a CLI arg, or an on-disk
    /// settings file -- see `CodexBackend`/`PiBackend`'s own overrides for
    /// which). Defaults to `false`, mirroring [`nudge`](Self::nudge)'s
    /// default-no-op-override shape: only a backend with a real mechanism
    /// needs to override it. `core::validate`'s `agent_supports_maximum_context`
    /// is the actual submit-time gate that keeps this field from reaching a
    /// backend that doesn't support it; this is the runner-side mirror of
    /// that same classification, checked defensively in `execute.rs` before
    /// a cell is ever run.
    fn supports_maximum_context(&self) -> bool {
        false
    }

    /// RAL-304: whether this backend has a real delivery mechanism for
    /// `RunOptions::auto_compact_threshold` -- an absolute token count at
    /// which auto-compaction should trigger (see
    /// `ClaudeCodeBackend`/`CodexBackend`/`PiBackend`'s own overrides for
    /// which mechanism each uses). Defaults to `false`, same shape as
    /// [`supports_maximum_context`](Self::supports_maximum_context); accepted
    /// by a wider set of backends than that method (claude-code overrides
    /// this one but not that one -- see
    /// `ralphus_core::schema::agent_supports_auto_compact_threshold`'s doc
    /// comment for why). `core::validate`'s
    /// `agent_supports_auto_compact_threshold` is the actual submit-time
    /// gate; this is the runner-side mirror, checked defensively in
    /// `execute.rs` before a cell is ever run.
    fn supports_auto_compact_threshold(&self) -> bool {
        false
    }

    /// RAL-333: whether this backend has a real delivery mechanism for
    /// `RunOptions::maximum_tool_output_tokens` -- a way to cap how many tokens a
    /// single tool-call output may inject into the agent's context (an env
    /// var, a CLI arg, or an on-disk settings file -- see
    /// `ClaudeCodeBackend`/`CodexBackend`/`PiBackend`'s own overrides for
    /// which). Defaults to `false`, same shape as
    /// [`supports_maximum_context`](Self::supports_maximum_context).
    /// `core::validate`'s `agent_supports_maximum_tool_output_tokens` is the
    /// actual submit-time gate; this is the runner-side mirror, checked
    /// defensively in `execute.rs` before a cell is ever run.
    fn supports_maximum_tool_output_tokens(&self) -> bool {
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct StubBackend;

    impl ModelBackend for StubBackend {
        fn run(
            &self,
            _prompt: &str,
            _workspace: &Workspace,
            _options: &RunOptions<'_>,
        ) -> Result<BackendOutcome, BackendError> {
            Ok(BackendOutcome::default())
        }
    }

    #[test]
    fn default_nudge_is_a_no_op_for_a_backend_that_does_not_override_it() {
        let backend = StubBackend;
        let ws = Workspace::create(std::env::temp_dir()).unwrap();
        let result = backend.nudge(&ws, &RunOptions::default());
        assert_eq!(result, Ok(None));
    }
}
