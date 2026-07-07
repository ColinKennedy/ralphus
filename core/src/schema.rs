//! Task-file schema: the serde types a submitted TOML batch deserializes into.
//!
//! Ported from the predecessor project (`old:src/tasks/schema.rs`) with one
//! deliberate fix: a session's `prompt` is now `Option<String>` and pairs with
//! `command`. In the old project `prompt` was a required `String` even though
//! the validator and UI allowed `command`-only sessions, so a valid
//! `command`-only session would pass validation and then fail to deserialize in
//! the runner (see `FINDINGS.local.md` §2.3). Making both optional and enforcing
//! "exactly one" in the validator keeps the type and the rules in agreement.

use serde::Deserialize;

/// Settings that apply to an entire submission (task file).
///
/// Declared via `[[default]]` blocks in TOML; only the first block is used.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct DefaultBlock {
    /// Other run IDs (or `run-id/task/session` paths) that must be Done before
    /// this submission is allowed to start.
    #[serde(default)]
    pub depends_on: Vec<String>,
}

/// A whole submitted task file.
#[derive(Debug, Clone, Deserialize)]
pub struct TaskFile {
    /// `[[default]]` blocks. Only the first is significant.
    #[serde(default, rename = "default")]
    pub defaults: Vec<DefaultBlock>,
    /// The tasks in this submission.
    #[serde(default)]
    pub task: Vec<TaskDef>,
}

/// One task: a unit of work made of one or more agent sessions plus verify steps.
#[derive(Debug, Clone, Deserialize)]
pub struct TaskDef {
    /// Task name / identifier (unique within a submission).
    pub name: String,
    /// Namespace label. Defaults to the git repo name of the first session's cwd.
    #[serde(default)]
    pub project: Option<String>,
    /// Agent binary for all sessions (e.g. `"claude"`). Sessions inherit unless overridden.
    #[serde(default)]
    pub agent: Option<String>,
    /// Default model for all sessions. Sessions may override.
    #[serde(default)]
    pub model: Option<String>,
    /// Extra CLI args passed to the agent for every session; task-level first.
    #[serde(default)]
    pub args: Vec<String>,
    /// Task budget cap in total tokens (input + output). A session exceeding it
    /// is failed. Sessions/verifies inherit this unless they set their own.
    #[serde(default)]
    pub budget_tokens: Option<u64>,
    /// Retry count.
    #[serde(default)]
    pub max_retries: Option<u32>,
    /// Default wall-clock timeout in minutes for the task's sessions and verify
    /// steps; each may override with its own `timeout_minutes`.
    #[serde(default)]
    pub timeout_minutes: Option<u32>,
    /// Other tasks/sessions this whole task waits on.
    #[serde(default)]
    pub depends_on: Vec<String>,
    /// The agent sessions.
    #[serde(default)]
    pub session: Vec<SessionDef>,
    /// Verify steps that run after ALL sessions complete.
    #[serde(default)]
    pub verify: Vec<VerifyStep>,
}

/// One agent session within a task.
#[derive(Debug, Clone, Deserialize)]
pub struct SessionDef {
    /// Session ID (for dependency references). Must not contain `/`.
    #[serde(default)]
    pub id: Option<String>,
    /// Human-readable display label. Shown in the board wherever sessions are
    /// listed; falls back to `id` when unset. Has no structural meaning.
    #[serde(default)]
    pub name: Option<String>,
    /// Agent-bus role.
    #[serde(default)]
    pub role: Option<String>,
    /// Working directory the agent operates in (required at validation time).
    #[serde(default)]
    pub cwd: Option<String>,
    /// AI-driven prompt. Mutually exclusive with `command`; exactly one is required.
    #[serde(default)]
    pub prompt: Option<String>,
    /// Deterministic shell command. Mutually exclusive with `prompt`.
    #[serde(default)]
    pub command: Option<String>,
    /// Sessions that must complete before this one starts. Within-task session
    /// ID (`"session-a"`) or cross-task `"<task-name>/<session-id>"`.
    #[serde(default)]
    pub depends_on: Vec<String>,
    /// Override the task-level agent for this session.
    #[serde(default)]
    pub agent: Option<String>,
    /// Override the task-level model for this session.
    #[serde(default)]
    pub model: Option<String>,
    /// Subdirectories of a monorepo this session is scoped to (e.g.
    /// `["packages/foo", "packages/bar"]`). At run time the daemon injects a
    /// system-prompt addendum instructing the agent to confine its edits to
    /// those paths. The `cwd` itself always points to the repo root (RAL-23).
    #[serde(default)]
    pub subprojects: Vec<String>,
    /// System-prompt text delivered to the agent as an *appended* system prompt
    /// (via the backend's own mechanism, e.g. the Claude Code CLI's
    /// `--append-system-prompt`) rather than concatenated into the user prompt.
    /// Session-level only; validation restricts it to the `claude-code` backend
    /// until the other backends' support is complete (RAL-5).
    #[serde(default)]
    pub system_prompt: Option<String>,
    /// Where [`system_prompt`](Self::system_prompt) is placed. The only accepted
    /// value today is [`SYSTEM_PROMPT_POSITION_APPEND`] (`"append"`); validation
    /// rejects any other value.
    #[serde(default)]
    pub system_prompt_position: Option<String>,
    /// Extra args appended after any task-level args for this session only.
    #[serde(default)]
    pub args: Vec<String>,
    /// Per-session budget in total tokens (input + output). Falls back to the
    /// task-level `budget_tokens` when unset. Exceeding it fails the session.
    #[serde(default)]
    pub budget_tokens: Option<u64>,
    /// Per-session wall-clock timeout in minutes. Falls back to the task-level
    /// `timeout_minutes` when unset.
    #[serde(default)]
    pub timeout_minutes: Option<u32>,
    /// Session-level verify steps.
    #[serde(default)]
    pub verify: Vec<VerifyStep>,
    /// Review (guardian) memberships for this session's worktree branch. Declared
    /// as `[[task.session.review]]`. Sessions whose worktrees resolve to the same
    /// project fold into one review; see `REVIEWS.local.md`.
    #[serde(default)]
    pub review: Vec<ReviewDef>,
    /// Upstream branch source for this session's branch. When set to
    /// `"<<task:task-name>>"` (or `"<<task:task-name/session-id>>"`), the daemon
    /// rebases this session's branch onto the named dependency's current branch
    /// tip immediately before starting the runner. Both worktrees must be in the
    /// same git repository; cross-repo upstreams are skipped with a warning.
    #[serde(default)]
    pub upstream: Option<String>,
}

/// Sentinel prefix for `upstream = "<<task:task-name>>"` or
/// `"<<task:task-name/session-id>>"`: rebase this session's branch onto the
/// named dependency's current branch tip before the session starts. The daemon
/// resolves this at run time, immediately before launching the runner.
pub const UPSTREAM_TASK_REF_PREFIX: &str = "<<task:";

/// The `base` sentinel meaning "use this worktree's upstream branch, resolved at
/// submit time; fail if the worktree has no upstream".
pub const REVIEW_BASE_UPSTREAM: &str = "<<upstream>>";

/// The scheme prefix for a review-link placeholder id. A review whose `id` is
/// `ralphus:new-review/<key>` declares a *stable link key*: every session — in
/// any task or any separate submission — that names the same `<key>` attaches to
/// one shared review, instead of grouping by project. The `<key>` is a
/// placeholder the daemon resolves to a real guardian at submit time.
pub const REVIEW_LINK_PREFIX: &str = "ralphus:new-review/";

/// If `id` is a review-link placeholder (`ralphus:new-review/<key>`), return its
/// `<key>` trimmed of surrounding whitespace. Returns `None` for a plain id or a
/// non-matching scheme, or when the key is empty.
#[must_use]
pub fn review_link_key(id: &str) -> Option<&str> {
    id.strip_prefix(REVIEW_LINK_PREFIX)
        .map(str::trim)
        .filter(|k| !k.is_empty())
}

/// A review (guardian) membership declared on a session via
/// `[[task.session.review]]`.
///
/// The session's worktree branch is included in the review for its project. When
/// several sessions across a run resolve to the same project they collapse into a
/// single review; when they span N projects the daemon materializes N reviews and
/// disambiguates their names. The `id`/`name`/`base` here are the *suggested*
/// starting point — the daemon may rename a review (e.g. `review-001`) when it has
/// to split one declaration across projects.
#[derive(Debug, Clone, Deserialize)]
pub struct ReviewDef {
    /// Human-readable review id (also the default label). Optional.
    #[serde(default)]
    pub id: Option<String>,
    /// GUI label; falls back to `id` when unset.
    #[serde(default)]
    pub name: Option<String>,
    /// Base branch the review rebases onto. The sentinel [`REVIEW_BASE_UPSTREAM`]
    /// (`<<upstream>>`) means "use the worktree's upstream" — resolved by the
    /// daemon at submit, and a hard error there if the worktree has none.
    #[serde(default)]
    pub base: Option<String>,
    /// Backend that resolves merge conflicts (and applies reviewer feedback) for
    /// this review, e.g. `"claude"` or `"ollama"`. Unset falls back to the
    /// `RALPHUS_RESOLVER_AGENT` env override, then `ollama`.
    #[serde(default)]
    pub agent: Option<String>,
    /// Model the resolver `agent` runs, e.g. `"qwen3:8b"`. Unset falls back to
    /// the `RALPHUS_RESOLVER_MODEL` env override, then `qwen3:8b` for the ollama
    /// backend (other backends take their own default).
    #[serde(default)]
    pub model: Option<String>,
}

/// One verify step. Exactly one of `command` / `brain` / `prompt` must be set.
#[derive(Debug, Clone, Deserialize)]
pub struct VerifyStep {
    /// Verify step ID (for `restart_on` / verify-level dependencies).
    #[serde(default)]
    pub id: Option<String>,
    /// Shell command; exit code is the verdict.
    #[serde(default)]
    pub command: Option<String>,
    /// Prompt routed to the local brain (deferred in ralphus MVP).
    #[serde(default)]
    pub brain: Option<String>,
    /// Headless AI verifier prompt. Runs using the owning session's
    /// resolved backend program (its `agent`), with this step's own `model`
    /// as an override — a verify step has no separate backend selector.
    #[serde(default)]
    pub prompt: Option<String>,
    /// Model override for the `prompt` verify (falls back to the owning
    /// session's resolved model when unset).
    #[serde(default)]
    pub model: Option<String>,
    /// Extra CLI args for the prompt-verifier invocation (e.g. `--append-system-prompt`).
    #[serde(default)]
    pub arguments: Vec<String>,
    /// Budget for the verify prompt, in total tokens (input + output). Falls
    /// back to the task-level `budget_tokens` when unset.
    #[serde(default)]
    pub budget_tokens: Option<u64>,
    /// Per-verify wall-clock timeout in minutes. Falls back to the task-level
    /// `timeout_minutes` when unset.
    #[serde(default)]
    pub timeout_minutes: Option<u32>,
    /// Whether the step needs human approval.
    #[serde(default)]
    pub requires_approval: bool,
    /// Other verify steps that, when they fire, re-run this session's verify
    /// cursor from the start. Grammar: `task/session/verify?on=pass|fail|both`,
    /// with wildcards `task/*` and `task/session/*`.
    #[serde(default)]
    pub restart_on: Vec<String>,
}

/// Resolved agent config after task→session inheritance is applied.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedAgent {
    /// Binary name or path to invoke.
    pub program: String,
    /// Effective model, if set.
    pub model: Option<String>,
    /// Extra args, task-level first then session-level.
    pub args: Vec<String>,
}

/// The default agent program when neither session nor task specifies one.
pub const DEFAULT_AGENT: &str = "claude";

/// The [`SessionDef::system_prompt_position`] value meaning "append to the
/// agent's system prompt". Currently the only accepted position (RAL-5).
pub const SYSTEM_PROMPT_POSITION_APPEND: &str = "append";

/// Whether `agent` is a backend with complete appended-system-prompt support.
///
/// Only the Claude Code CLI (`claude-code`, and its `claude-cli` alias) maps the
/// appended system prompt to a real backend flag (`--append-system-prompt`)
/// today. Support for the other backends is best-effort but not complete, so
/// validation rejects `system_prompt`/`system_prompt_position` for any other
/// agent (RAL-5).
#[must_use]
pub fn agent_supports_system_prompt(agent: &str) -> bool {
    matches!(agent, "claude-code" | "claude-cli")
}

impl ResolvedAgent {
    /// Merge task defaults with session overrides (session wins).
    #[must_use]
    pub fn resolve(task: &TaskDef, session: &SessionDef) -> Self {
        let program = session
            .agent
            .clone()
            .or_else(|| task.agent.clone())
            .unwrap_or_else(|| DEFAULT_AGENT.to_string());
        let model = session.model.clone().or_else(|| task.model.clone());
        let mut args = task.args.clone();
        args.extend_from_slice(&session.args);
        Self {
            program,
            model,
            args,
        }
    }

    /// Task-level defaults only (for task-level verify steps that have no session).
    #[must_use]
    pub fn from_task(task: &TaskDef) -> Self {
        Self {
            program: task
                .agent
                .clone()
                .unwrap_or_else(|| DEFAULT_AGENT.to_string()),
            model: task.model.clone(),
            args: task.args.clone(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn task_with(agent: Option<&str>, model: Option<&str>, args: &[&str]) -> TaskDef {
        TaskDef {
            name: "t".into(),
            project: None,
            agent: agent.map(str::to_string),
            model: model.map(str::to_string),
            args: args.iter().map(|s| (*s).to_string()).collect(),
            budget_tokens: None,
            max_retries: None,
            timeout_minutes: None,
            depends_on: vec![],
            session: vec![],
            verify: vec![],
        }
    }

    fn session_with(agent: Option<&str>, model: Option<&str>, args: &[&str]) -> SessionDef {
        SessionDef {
            id: None,
            name: None,
            role: None,
            cwd: Some("/tmp".into()),
            subprojects: vec![],
            prompt: Some("hi".into()),
            command: None,
            depends_on: vec![],
            agent: agent.map(str::to_string),
            model: model.map(str::to_string),
            system_prompt: None,
            system_prompt_position: None,
            args: args.iter().map(|s| (*s).to_string()).collect(),
            budget_tokens: None,
            timeout_minutes: None,
            verify: vec![],
            review: vec![],
            upstream: None,
        }
    }

    #[test]
    fn session_overrides_task_agent_and_model() {
        let task = task_with(Some("claude"), Some("task-model"), &["--task"]);
        let sess = session_with(Some("aider"), Some("sess-model"), &["--sess"]);
        let r = ResolvedAgent::resolve(&task, &sess);
        assert_eq!(r.program, "aider");
        assert_eq!(r.model.as_deref(), Some("sess-model"));
        assert_eq!(r.args, vec!["--task", "--sess"]);
    }

    #[test]
    fn session_inherits_task_when_unset() {
        let task = task_with(Some("codex"), Some("m"), &["--a"]);
        let sess = session_with(None, None, &[]);
        let r = ResolvedAgent::resolve(&task, &sess);
        assert_eq!(r.program, "codex");
        assert_eq!(r.model.as_deref(), Some("m"));
        assert_eq!(r.args, vec!["--a"]);
    }

    #[test]
    fn defaults_to_claude_when_nothing_set() {
        let task = task_with(None, None, &[]);
        let sess = session_with(None, None, &[]);
        assert_eq!(ResolvedAgent::resolve(&task, &sess).program, DEFAULT_AGENT);
    }

    #[test]
    fn session_review_deserializes() {
        let toml = r#"
            [[task]]
            name = "t"
            [[task.session]]
            cwd = "/repo/.wt/feat"
            prompt = "do work"
            [[task.session.review]]
            id = "backend"
            name = "Backend review"
            base = "<<upstream>>"
            agent = "claude"
            model = "claude-opus-4-8"
        "#;
        let parsed: TaskFile = toml::from_str(toml).expect("should deserialize");
        let review = &parsed.task[0].session[0].review;
        assert_eq!(review.len(), 1);
        assert_eq!(review[0].id.as_deref(), Some("backend"));
        assert_eq!(review[0].name.as_deref(), Some("Backend review"));
        assert_eq!(review[0].base.as_deref(), Some(REVIEW_BASE_UPSTREAM));
        assert_eq!(review[0].agent.as_deref(), Some("claude"));
        assert_eq!(review[0].model.as_deref(), Some("claude-opus-4-8"));
    }

    #[test]
    fn system_prompt_fields_deserialize() {
        let toml = r#"
            [[task]]
            name = "t"
            [[task.session]]
            cwd = "/repo"
            prompt = "do work"
            agent = "claude-code"
            system_prompt = "Follow the house style guide."
            system_prompt_position = "append"
        "#;
        let parsed: TaskFile = toml::from_str(toml).expect("should deserialize");
        let sess = &parsed.task[0].session[0];
        assert_eq!(
            sess.system_prompt.as_deref(),
            Some("Follow the house style guide.")
        );
        assert_eq!(
            sess.system_prompt_position.as_deref(),
            Some(SYSTEM_PROMPT_POSITION_APPEND)
        );
    }

    #[test]
    fn system_prompt_fields_default_to_none() {
        let toml = r#"
            [[task]]
            name = "t"
            [[task.session]]
            cwd = "/repo"
            prompt = "do work"
        "#;
        let parsed: TaskFile = toml::from_str(toml).expect("should deserialize");
        let sess = &parsed.task[0].session[0];
        assert!(sess.system_prompt.is_none());
        assert!(sess.system_prompt_position.is_none());
    }

    #[test]
    fn command_only_session_deserializes() {
        // The old project's bug: this used to fail because `prompt` was required.
        let toml = r#"
            [[task]]
            name = "t"
            [[task.session]]
            cwd = "/tmp"
            command = "cargo build"
        "#;
        let parsed: TaskFile = toml::from_str(toml).expect("should deserialize");
        let sess = &parsed.task[0].session[0];
        assert!(sess.prompt.is_none());
        assert_eq!(sess.command.as_deref(), Some("cargo build"));
    }
}
