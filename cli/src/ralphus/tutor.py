"""The Task TOML tutorial shown by ``ralphus task show-tutor``.

A single reference string, kept close to ralphus's actual schema (see
``core/src/schema.rs`` and ``core/src/validate.rs``). When the schema changes,
update this text too.

The text is intentionally ASCII-only: `print` on a legacy Windows console
(cp1252) raises UnicodeEncodeError on box-drawing characters.
"""

from __future__ import annotations

__all__ = ["TASK_TUTOR", "task_tutor"]

TASK_TUTOR = r"""
===============================================================
             ralphus  Task TOML -- Schema Reference
===============================================================

A Task TOML describes one or more interdependent sessions (AI
prompts and/or deterministic shell commands) to run under the
ralphus runner. Every file must contain at least one [[task]].

Section headers use TOML array-of-tables syntax ([[...]]). Each
[[task]] starts a new task; each [[task.session]] appends to the
most recent [[task]]; each [[task.session.verify]] / [[task.session.review]]
appends to the most recent [[task.session]]; and so on.

Tip: validate before submitting -- `ralphus validate file.toml`
(runs offline, no daemon needed).

---------------------------------------------------------------
 [[default]]   (optional; only the first block is used)
---------------------------------------------------------------
 Key         Type           Notes
 depends_on  array<string>  Cross-run gates: other run-ids (or
                            "run-id/task/session" paths) that must be
                            Done before THIS whole submission starts.

---------------------------------------------------------------
 [[task]]   (required, one or more)
---------------------------------------------------------------
 Key          Type           Notes / example
 name         string  REQ    Unique task name, e.g. "build"
 project      string         Namespace label (defaults to the git repo name)
 agent        string         Default agent for the task's sessions
 model        string         Default model for the task's sessions
 args         array<string>  Extra agent CLI flags for every session
 budget_usd   number         Spend cap
 max_retries  integer        Auto-retry count on failure
 timeout_min  integer        Session wall-clock timeout, minutes
 depends_on   array<string>  Tasks this task waits on (see formats below)

---------------------------------------------------------------
 [[task.session]]   (zero or more per task)
---------------------------------------------------------------
 Exactly ONE of `prompt` or `command` is required.

 Key         Type           Notes
 id          string         Stable id for dependency references (no "/")
 cwd         string  REQ    Absolute path to the session's working dir --
                            typically the git WORKTREE the agent operates in.
                            On Windows use forward slashes (e.g.
                            "C:/Users/me/repo/.wt/feat") -- a backslash after
                            C:\U... is an invalid TOML escape.
 prompt      string  ONE-OF AI prompt text (launches an agent session).
                            May contain {handoff:<task-or-session>} -- replaced
                            at run time with the summaries of this session's
                            completed dependencies.
 command     string  ONE-OF Deterministic shell command (no AI). Exit 0 = ok.
                            Mutually exclusive with `prompt`. NOTE: command
                            sessions ignore `agent` and `model` entirely.
 role        string         Optional role label
 agent       string         Override the task agent for this session
 model       string         Override the task model for this session
 args        array<string>  Per-session agent flags (appended after task args)
 budget_usd  number         Per-session spend cap
 depends_on  array<string>  Sessions that must finish first (see formats below)

 Agent selection:
   agent = "claude"   (default) -- Anthropic. Uses your Claude subscription via
                       claude-code, or the API if ANTHROPIC_API_KEY is set.
   agent = "claude-code" -- force the subscription CLI path (no API key needed).
   agent = "ollama"   -- local model; pair with e.g. model = "qwen3:8b"
                        (pick a tool-capable model).

---------------------------------------------------------------
 [[task.session.review]]   (zero or more per session)
---------------------------------------------------------------
 Marks this session's worktree branch for inclusion in a review
 (a Guardian). At submit, sessions are grouped by PROJECT (the
 shared git dir of their worktree): all worktrees of one repo fold
 into one review; sessions spanning N projects make N reviews, with
 names disambiguated automatically. Branch order follows the task/
 session dependency order.

 Key    Type    Notes
 base   string  Branch the review rebases onto. Usually "<<upstream>>",
                meaning "use this worktree branch's upstream" -- a submit-time
                error if the worktree has no upstream. May be a literal branch.
 id     string  Human-readable review id (also the default label).
 name   string  GUI label; falls back to `id` when unset.

---------------------------------------------------------------
 [[task.verify]]  and  [[task.session.verify]]   (zero or more)
---------------------------------------------------------------
 [[task.verify]] runs after ALL of a task's sessions complete.
 [[task.session.verify]] runs after its own session finishes.
 Exactly ONE of `command`, `brain`, or `agent` is required.

 Key                Type           Notes
 id                 string         Stable id for restart_on references
 command            string  ONE-OF Shell command; exit 0 = PASS. (Works today.)
 brain              string  ONE-OF Local-LLM check (planned; not yet run in MVP).
 agent              string  ONE-OF Sub-agent verifier prompt (planned in MVP).
 model              string         Model for an `agent` verifier
 arguments          array<string>  Extra flags for the agent verifier, e.g.
                                   ["--append-system-prompt", "..."]
 budget_usd         number         Spend cap for the agent verifier
 requires_approval  boolean        Pause for human sign-off (planned)
 restart_on         array<string>  Re-run this task when a referenced verify
                                   step fires. Grammar:
                                   "task/session/verify?on=pass|fail|both",
                                   with "task/*" and "task/session/*" wildcards.

 NOTE: today the runner executes `command` verifiers; brain/agent/approval
 verifiers are accepted by the schema but deferred, and stay pending.

---------------------------------------------------------------
 depends_on formats
---------------------------------------------------------------
   "session-id"              another session in the SAME task
   "task-name/session-id"    a session in another (earlier) task
   "task-name"               (task-level) the whole named task
   "run-id"                  ([[default]]) another run, gated on it being Done
   "run-id/task/session"     ([[default]]) a specific path in another run

===============================================================
                          EXAMPLES
===============================================================

-- 1. Hello, world --------------------------------------------

[[task]]
name = "hello"

  [[task.session]]
  cwd    = "C:/Users/me/repo"
  prompt = "Print 'Hello, World!' to a new file hello.txt."

-- 2. A local (Ollama) deterministic check --------------------

[[task]]
name = "check"

  [[task.session]]
  cwd     = "C:/Users/me/repo"
  agent   = "ollama"
  model   = "qwen3:8b"
  command = "cargo test"          # command ignores agent/model anyway

-- 3. Two tasks, a handoff, and a per-project review ----------

[[task]]
name = "setup"

  [[task.session]]
  id     = "init"
  cwd    = "C:/Users/me/repo/.wt/feature-a"
  prompt = "Create data.json with {\"version\": 1}."

    # this worktree's branch joins its project's review
    [[task.session.review]]
    id   = "backend"
    base = "<<upstream>>"

    [[task.session.verify]]
    command = "test -f data.json"

[[task]]
name       = "report"
depends_on = ["setup"]            # waits for all of setup's sessions + verifiers

  [[task.session]]
  cwd    = "C:/Users/me/repo/.wt/feature-b"
  prompt = "Using {handoff:setup}, write a summary to report.md."

    [[task.session.review]]
    id   = "backend"
    base = "<<upstream>>"

    [[task.session.verify]]
    command = "test -f report.md"

Submit it:  ralphus submit tasks.toml
(Adds it as Pending -- schedulable now. Use --hold to stage as Queued.)
"""


def task_tutor() -> str:
    """Return the Task TOML tutorial text."""
    return TASK_TUTOR
