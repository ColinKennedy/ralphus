//! RAL-297: background "generation step" jobs backing the Simple task
//! form's opt-in "Generate Proofs" / "Generate Manual Checks" / "Generate
//! Auto-Build Steps" buttons.
//!
//! Each is a single one-shot LLM call (reusing `crate::runner`'s
//! `Runner`/`RunnerSpec` plumbing exactly as `crate::pr::synthesize_pr_text`
//! and `crate::guardian_merge::generate_manual_commands` already do) whose
//! prompt asks for a structured JSON list, parsed tolerantly (fenced code
//! blocks, chatty preambles) the same way
//! `guardian_merge::parse_manual_commands_response` does. The board's
//! editable-list widget merges the result in.
//!
//! Deliberately fire-and-forget-plus-poll (`POST /api/generate` kicks off a
//! background thread and returns `202` immediately; `GET /api/generate/{id}`
//! polls the result) rather than blocking the HTTP response on the call --
//! `crate::server`'s `ReadPool` doc comment spells out why: **every mutating
//! request runs on the accept loop, one at a time**, so a handler that
//! blocked there for the seconds-to-minutes a tmux-wrapped agent subprocess
//! can take would stall every other write (submit, cancel, activate, ...)
//! for that whole window. This module follows the same pattern as
//! `crate::server::guardian_resolve_input`.
//!
//! `POST /api/generate/{id}/cancel` really does kill the in-flight agent
//! subprocess, not just tell the board to stop caring about the result: the
//! background thread registers a [`crate::cancel::CancelToken`] for the job
//! id in the daemon's shared `Cancellations` registry (the exact mechanism
//! `crate::scheduler` uses for a squad's own cells) before calling
//! [`run_generation`], and the cancel endpoint trips that same token.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};

use crate::cancel::CancelToken;
use crate::runner::{Runner, RunnerSpec, SubprocessRunner};

/// One proposed proof step or manual check, generic enough for the board's
/// single reusable editable-list widget to render either kind.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GeneratedItem {
    /// Short human-readable id/label (e.g. a proof step's `id`, or a manual
    /// check's button label).
    pub label: String,
    /// The shell command (or, for a manual check, prompt-style hint) this
    /// row represents.
    pub value: String,
}

/// The state of one generation job, as seen by `GET /api/generate/{id}`.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "status", rename_all = "lowercase")]
pub enum GenerationJob {
    Running,
    Done { items: Vec<GeneratedItem> },
    Error { message: String },
}

/// In-memory registry of generation jobs, owned by `Daemon`. Jobs are never
/// pruned -- the board polls once per user click and the process-lifetime
/// volume is tiny (a handful of ids per running daemon session), so a
/// time/count-based eviction policy (like `crate::config::CartographerConfig`'s)
/// isn't worth the complexity yet.
#[derive(Clone, Default)]
pub struct GenerationJobs(Arc<Mutex<HashMap<String, GenerationJob>>>);

impl GenerationJobs {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a new `Running` job and return its id.
    #[must_use]
    pub fn start(&self) -> String {
        let id = format!("gen-{}", crate::runner::generate_agent_session_id());
        self.0
            .lock()
            .expect("generation jobs mutex poisoned")
            .insert(id.clone(), GenerationJob::Running);
        id
    }

    /// The current state of job `id`, or `None` if no such job was ever
    /// started (an unknown/typo'd id, not "still running").
    #[must_use]
    pub fn get(&self, id: &str) -> Option<GenerationJob> {
        self.0
            .lock()
            .expect("generation jobs mutex poisoned")
            .get(id)
            .cloned()
    }

    /// Record the terminal state of job `id`.
    pub fn finish(&self, id: &str, job: GenerationJob) {
        self.0
            .lock()
            .expect("generation jobs mutex poisoned")
            .insert(id.to_string(), job);
    }
}

/// A `POST /api/generate` request: which kind of list to propose, and the
/// agent/model/cwd/context to run it against.
#[derive(Debug, Clone, Deserialize)]
pub struct GenerateRequest {
    /// `"proof_steps"`, `"manual_checks"`, or `"auto_build_steps"` -- see
    /// [`GenerationKind`].
    pub kind: String,
    pub cwd: String,
    pub agent: String,
    #[serde(default)]
    pub model: Option<String>,
    /// The Simple form's own `prompt` field (plus any template
    /// substitution already applied), used to ground the proposal in what
    /// the task is actually meant to do.
    pub prompt_context: String,
}

/// The generation-step flavors the Simple form offers. All share the same
/// JSON-list prompt/parse machinery; only the wording differs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GenerationKind {
    ProofSteps,
    ManualChecks,
    AutoBuildSteps,
}

impl GenerationKind {
    #[must_use]
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "proof_steps" => Some(Self::ProofSteps),
            "manual_checks" => Some(Self::ManualChecks),
            "auto_build_steps" => Some(Self::AutoBuildSteps),
            _ => None,
        }
    }

    /// The `label`/`value` semantics described to the model, and to the
    /// board (which reuses this to label the editable-list widget's columns).
    #[must_use]
    fn describe(self) -> &'static str {
        match self {
            Self::ProofSteps => {
                "each item's \"value\" must be a single shell command that verifies the \
                 change (e.g. a test or lint command runnable in this repository)"
            }
            Self::ManualChecks => {
                "each item's \"value\" must be a short instruction describing something a \
                 human reviewer should manually check or try"
            }
            Self::AutoBuildSteps => {
                "each item's \"value\" must be a single shell command that builds/compiles \
                 the change (run automatically when this review's branches merge)"
            }
        }
    }
}

/// Build the (system_prompt, user_prompt) pair for one generation call.
/// Mirrors `guardian_merge::manual_commands_prompt`'s "return ONLY a valid
/// JSON array, no markdown fences, no explanation" phrasing, which is
/// already proven to work against small local models.
#[must_use]
fn generation_prompt(kind: GenerationKind, prompt_context: &str) -> (String, String) {
    let system_prompt = "You propose short, concrete checklist items for a software task. \
                          Respond with ONLY valid JSON, no markdown fences, no prose."
        .to_string();
    let user_prompt = format!(
        "A task is about to run with this prompt:\n\n{prompt_context}\n\nPropose 1-5 items. \
         {desc}. Each item also needs a short \"label\" (a few words). Return ONLY a JSON \
         array of the form [{{\"label\": \"...\", \"value\": \"...\"}}] -- no markdown \
         fences, no explanation, no other text.",
        desc = kind.describe()
    );
    (system_prompt, user_prompt)
}

/// Parse a (possibly fenced/chatty) model response into a list of
/// [`GeneratedItem`]s. Tries a direct JSON array parse first, then falls
/// back to slicing out the outermost `[...]` substring for a model that
/// wrapped its answer in prose or a code fence -- the same tolerance
/// `guardian_merge::parse_manual_commands_response` applies to its own
/// JSON-object variant.
#[must_use]
pub fn parse_generated_items(text: &str) -> Option<Vec<GeneratedItem>> {
    let text = text.trim();
    if let Ok(items) = serde_json::from_str::<Vec<GeneratedItem>>(text) {
        return Some(items);
    }
    let start = text.find('[')?;
    let end = text.rfind(']')?;
    if end <= start {
        return None;
    }
    serde_json::from_str::<Vec<GeneratedItem>>(&text[start..=end]).ok()
}

/// Run one generation call synchronously (called from a background thread
/// spawned by the `POST /api/generate` handler -- never call this directly
/// from an HTTP handler on the accept loop, see the module doc comment).
/// `cancel` is the token registered for this job's id in `server::
/// generate_start`; `POST /api/generate/{id}/cancel` trips it, and
/// `run_cancellable` polls it and kills the agent subprocess -- the same
/// mechanism (`crate::cancel`) a squad's own cell cancellation uses. A
/// tripped token surfaces here as an ordinary `RunnerResult::failure
/// ("cancelled")`, which the `!result.is_done()` branch below turns into a
/// `GenerationJob::Error` like any other failure -- no separate "cancelled"
/// status is needed since the board never renders a generation job's result
/// once its owning New Task modal has been closed.
#[must_use]
pub fn run_generation(req: &GenerateRequest, cancel: &CancelToken) -> GenerationJob {
    let Some(kind) = GenerationKind::parse(&req.kind) else {
        return GenerationJob::Error {
            message: format!(
                "unknown generation kind \"{}\" (expected \"proof_steps\", \"manual_checks\", \
                 or \"auto_build_steps\")",
                req.kind
            ),
        };
    };
    let (system_prompt, prompt) = generation_prompt(kind, &req.prompt_context);
    let spec = RunnerSpec {
        squad_id: "generate".to_string(),
        task: "generate".to_string(),
        cell_id: req.kind.clone(),
        cwd: req.cwd.clone(),
        prompt: Some(prompt),
        command: None,
        agent: req.agent.clone(),
        executable: None,
        model: req.model.clone(),
        system_prompt: Some(system_prompt),
        system_prompt_position: Some("append".to_string()),
        timeout_sec: Some(120),
        budget_tokens: None,
        maximum_budget_usd: None,
        maximum_context: None,
        auto_compact_threshold: None,
        maximum_tool_output_tokens: None,
        proof: false,
        trace_context: None,
        resume_agent_session_id: None,
        assigned_agent_session_id: None,
        env_overrides: std::collections::BTreeMap::new(),
        machine: None,
        tool_arg_truncate_chars: None,
        thrash_max_compactions: None,
        thrash_min_turn_gap: None,
        allow_personal_settings: false,
        allow_personal_memory: false,
    };
    let runner = SubprocessRunner::from_env();
    let result = runner.run_cancellable(&spec, cancel);
    if !result.is_done() {
        return GenerationJob::Error {
            message: result
                .error
                .unwrap_or_else(|| "generation call did not complete".to_string()),
        };
    }
    match parse_generated_items(&result.summary) {
        Some(items) => GenerationJob::Done { items },
        None => GenerationJob::Error {
            message: "the agent's response was not a valid JSON list of items".to_string(),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_a_clean_json_array() {
        let items =
            parse_generated_items(r#"[{"label": "fmt", "value": "cargo fmt --all -- --check"}]"#)
                .expect("valid json");
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].label, "fmt");
    }

    #[test]
    fn parses_a_fenced_and_chatty_response() {
        let items = parse_generated_items(
            "Sure, here you go:\n```json\n[{\"label\": \"test\", \"value\": \"cargo test\"}]\n```\nHope that helps!",
        )
        .expect("valid json inside prose");
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].value, "cargo test");
    }

    #[test]
    fn rejects_garbage() {
        assert!(parse_generated_items("not json at all").is_none());
    }

    #[test]
    fn generation_kind_parse_roundtrip() {
        assert_eq!(
            GenerationKind::parse("proof_steps"),
            Some(GenerationKind::ProofSteps)
        );
        assert_eq!(
            GenerationKind::parse("manual_checks"),
            Some(GenerationKind::ManualChecks)
        );
        assert_eq!(
            GenerationKind::parse("auto_build_steps"),
            Some(GenerationKind::AutoBuildSteps)
        );
        assert_eq!(GenerationKind::parse("bogus"), None);
    }

    #[test]
    fn run_generation_rejects_unknown_kind_without_touching_the_runner() {
        let req = GenerateRequest {
            kind: "bogus".to_string(),
            cwd: ".".to_string(),
            agent: "ollama".to_string(),
            model: None,
            prompt_context: "do the thing".to_string(),
        };
        match run_generation(&req, &CancelToken::never()) {
            GenerationJob::Error { message } => {
                assert!(message.contains("unknown generation kind"))
            }
            other => panic!("expected an error, got {other:?}"),
        }
    }

    #[test]
    fn jobs_registry_starts_running_then_reports_finish() {
        let jobs = GenerationJobs::new();
        let id = jobs.start();
        assert!(matches!(jobs.get(&id), Some(GenerationJob::Running)));
        jobs.finish(
            &id,
            GenerationJob::Done {
                items: vec![GeneratedItem {
                    label: "x".to_string(),
                    value: "y".to_string(),
                }],
            },
        );
        assert!(matches!(jobs.get(&id), Some(GenerationJob::Done { .. })));
        assert!(jobs.get("no-such-id").is_none());
    }
}
