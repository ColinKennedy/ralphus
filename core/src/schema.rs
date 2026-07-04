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
    /// Task budget cap in USD.
    #[serde(default)]
    pub budget_usd: Option<f64>,
    /// Retry count.
    #[serde(default)]
    pub max_retries: Option<u32>,
    /// Timeout in minutes.
    #[serde(default)]
    pub timeout_min: Option<u32>,
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
    /// Extra args appended after any task-level args for this session only.
    #[serde(default)]
    pub args: Vec<String>,
    /// Per-session budget in USD.
    #[serde(default)]
    pub budget_usd: Option<f64>,
    /// Session-level verify steps.
    #[serde(default)]
    pub verify: Vec<VerifyStep>,
    /// Review (guardian) memberships for this session's worktree branch. Declared
    /// as `[[task.session.review]]`. Sessions whose worktrees resolve to the same
    /// project fold into one review; see `REVIEWS.local.md`.
    #[serde(default)]
    pub review: Vec<ReviewDef>,
}

/// The `base` sentinel meaning "use this worktree's upstream branch, resolved at
/// submit time; fail if the worktree has no upstream".
pub const REVIEW_BASE_UPSTREAM: &str = "<<upstream>>";

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
}

/// One verify step. Exactly one of `command` / `brain` / `agent` must be set.
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
    /// Headless agent prompt; binary/model taken from this step's own fields.
    #[serde(default)]
    pub agent: Option<String>,
    /// Model for the `agent` verify.
    #[serde(default)]
    pub model: Option<String>,
    /// Extra CLI args for the agent invocation (e.g. `--append-system-prompt`).
    #[serde(default)]
    pub arguments: Vec<String>,
    /// Budget for the verify agent.
    #[serde(default)]
    pub budget_usd: Option<f64>,
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
            budget_usd: None,
            max_retries: None,
            timeout_min: None,
            depends_on: vec![],
            session: vec![],
            verify: vec![],
        }
    }

    fn session_with(agent: Option<&str>, model: Option<&str>, args: &[&str]) -> SessionDef {
        SessionDef {
            id: None,
            role: None,
            cwd: Some("/tmp".into()),
            prompt: Some("hi".into()),
            command: None,
            depends_on: vec![],
            agent: agent.map(str::to_string),
            model: model.map(str::to_string),
            args: args.iter().map(|s| (*s).to_string()).collect(),
            budget_usd: None,
            verify: vec![],
            review: vec![],
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
        "#;
        let parsed: TaskFile = toml::from_str(toml).expect("should deserialize");
        let review = &parsed.task[0].session[0].review;
        assert_eq!(review.len(), 1);
        assert_eq!(review[0].id.as_deref(), Some("backend"));
        assert_eq!(review[0].name.as_deref(), Some("Backend review"));
        assert_eq!(review[0].base.as_deref(), Some(REVIEW_BASE_UPSTREAM));
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
