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
    /// [`ModelBackend::supports_context_limits`] returns `true` -- `core`'s
    /// `agent_supports_maximum_context` validation gate rejects it for
    /// anything else before submission.
    pub maximum_context: Option<u64>,
    /// RAL-304: resolved auto-compact trigger threshold in tokens, or `None`
    /// for no explicit threshold. Same support restriction as
    /// [`Self::maximum_context`].
    pub auto_compact_threshold: Option<u64>,
    /// RAL-303: how many characters of a `tool_use` argument value to render
    /// into the Live View tmux pane before truncating, resolved daemon-side
    /// from `[live_view] tool_arg_truncate_chars`. Support is optional per
    /// backend -- only the claude-code backend reads this today; others
    /// ignore it, mirroring `assigned_agent_session_id`'s "hand-rolled
    /// backends accept and ignore it" precedent. `None` means the reading
    /// backend should fall back to its own default.
    pub tool_arg_truncate_chars: Option<u32>,
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
    /// `RunOptions::maximum_context`/`RunOptions::auto_compact_threshold`
    /// (an env var, a CLI arg, or an on-disk settings file -- see
    /// `ClaudeCodeBackend`/`CodexBackend`/`PiBackend`'s own overrides for
    /// which). Defaults to `false`, mirroring [`nudge`](Self::nudge)'s
    /// default-no-op-override shape: only a backend with a real mechanism
    /// needs to override it. `core::validate`'s `agent_supports_maximum_context`
    /// is the actual submit-time gate that keeps these fields from reaching a
    /// backend that doesn't support them; this is the runner-side mirror of
    /// that same classification, checked defensively in `execute.rs` before
    /// a cell is ever run.
    fn supports_context_limits(&self) -> bool {
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
