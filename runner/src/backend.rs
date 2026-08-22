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
#[derive(Debug, Clone, Default)]
pub struct BackendOutcome {
    pub summary: String,
    pub tokens_in: i64,
    pub tokens_out: i64,
    pub cost_usd: f64,
    pub agent_session_id: Option<String>,
}

/// Options common to every backend's `run` call, bundled to keep the trait's
/// signature from growing a new positional parameter every time a backend
/// needs one more piece of context.
#[derive(Debug, Clone, Default)]
pub struct RunOptions<'a> {
    pub model: Option<&'a str>,
    pub append_system_prompt: Option<&'a str>,
    pub resume_agent_session_id: Option<&'a str>,
    pub timeout_sec: Option<u64>,
}

pub trait ModelBackend {
    fn run(
        &self,
        prompt: &str,
        workspace: &Workspace,
        options: &RunOptions<'_>,
    ) -> Result<BackendOutcome, BackendError>;
}
