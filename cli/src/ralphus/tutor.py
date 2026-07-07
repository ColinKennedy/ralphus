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
 args           array<string>  Extra agent CLI flags for every session
 budget_tokens  integer        Default total-token cap (in+out); sessions/verifies
                               inherit it. Exceeding it fails the step.
 max_retries    integer        Auto-retry count on failure
 timeout_minutes integer       Default wall-clock timeout (minutes) for sessions
                               and verifies; each may override its own.
 depends_on     array<string>  Tasks this task waits on (see formats below)

---------------------------------------------------------------
 [[task.session]]   (zero or more per task)
---------------------------------------------------------------
 Exactly ONE of `prompt` or `command` is required.

 Key                     Type           Notes
 id                      string         Stable id for dependency references (no "/")
 name                    string         Human-readable display label shown in the board
                                        (card and detail pane). Falls back to `id` when
                                        unset. No structural meaning -- safe to rename.
 cwd                     string  REQ    Absolute path to the session's working dir --
                                        typically the git WORKTREE the agent operates in.
                                        On Windows use forward slashes (e.g.
                                        "C:/Users/me/repo/.wt/feat") -- a backslash after
                                        C:\U... is an invalid TOML escape.
 subprojects             array<string>  If your project lives inside a monorepo, declare
                                        which package subdirectories this session focuses
                                        on, e.g. subprojects = ["packages/foo", "libs/bar"].
                                        ralphus injects a system-prompt addendum telling
                                        the agent to scope its edits to those paths (the
                                        agent can still see the whole repo). The `cwd`
                                        should always point to the repo root. Omit (or
                                        leave empty) for single-project repos.
 prompt                  string  ONE-OF AI prompt text (launches an agent session).
                                        May contain {handoff:<task-or-session>} -- replaced
                                        at run time with the summaries of this session's
                                        completed dependencies.
 command                 string  ONE-OF Deterministic shell command (no AI). Exit 0 = ok.
                                        Mutually exclusive with `prompt`. NOTE: command
                                        sessions ignore `agent` and `model` entirely.
 role                    string         Optional role label
 agent                   string         Override the task agent for this session
 model                   string         Override the task model for this session
 system_prompt           string         Appended to the agent's system prompt before the
                                        session starts. More authoritative than embedding
                                        the same text inside `prompt`: agents can
                                        de-prioritise or forget instructions buried in a
                                        long user-turn prompt, but system-prompt text is
                                        treated as a hard constraint throughout the session.
                                        Use this for immutable safety rails such as
                                        "Do NOT commit and do NOT push." RESTRICTION:
                                        only valid with agent = "claude-code" (or when the
                                        task or global default resolves to "claude-code").
 system_prompt_position  string         Where `system_prompt` is injected. The only
                                        accepted value today is "append" (appended after
                                        the agent's built-in system prompt). Required
                                        whenever `system_prompt` is set.
 args                    array<string>  Per-session agent flags (appended after task args)
 budget_tokens           integer        Per-session total-token cap (falls back to task)
 timeout_minutes         integer        Per-session wall-clock timeout (falls back to task)
 depends_on              array<string>  Sessions that must finish first (see formats below)
 upstream               string         Branch-chaining sentinel. Set to
                                        "<<task:task-name>>" or
                                        "<<task:task-name/session-id>>" and
                                        the scheduler rebases THIS session's
                                        branch onto that dependency's current
                                        branch tip immediately before starting
                                        the runner. Use whenever this task picks
                                        up where a prior task left off in the
                                        same repo. Cross-repo upstreams are
                                        silently skipped with a warning.
                                        THIS IS THE DEFAULT RECOMMENDATION for
                                        sequential tasks in the same repo --
                                        prefer it over rebasing manually in the
                                        prompt or finalize session.

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
 (a Guardian). Branch order follows the task/session dependency order.

 Two ways branches fold into a review:
   * By PROJECT (default): sessions with a plain `id` are grouped by the
     shared git dir of their worktree -- all worktrees of one repo fold
     into one review; sessions spanning N projects make N reviews, with
     names disambiguated automatically. This grouping is per submission.
   * By LINK KEY (recommended for a planned batch): set
     id = "ralphus:new-review/<key>". Every session that repeats the same
     <key> -- across tasks AND across separate `ralphus submit` calls or
     separate .toml files -- attaches to ONE shared review. Use this when
     several tickets/branches belong in a single review together.

 Key    Type    Notes
 base   string  Branch the review rebases onto. Usually "<<upstream>>",
                meaning "use this worktree branch's upstream" -- a submit-time
                error if the worktree has no upstream. May be a literal branch.
 id     string  Human-readable review id (also the default label), OR a link
                placeholder "ralphus:new-review/<key>" (see above) whose <key>
                is letters/digits/-/_/. -- the daemon resolves it to a real
                review and reuses it for every session naming the same <key>.
 name   string  GUI label; falls back to `id` when unset. For a link id, set
                `name` so the review has a readable label (not the raw URI).
 agent  string  Backend that RESOLVES this review's merge conflicts (and applies
                reviewer feedback), e.g. "claude" or "ollama". This is the
                conflict-fixer, NOT the session that produced the branch. Unset
                falls back to the RALPHUS_RESOLVER_AGENT env override, then
                "ollama". Shown on the review page in the GUI.
 model  string  Model the resolver `agent` runs, e.g. "qwen3:8b". Unset falls
                back to RALPHUS_RESOLVER_MODEL, then "qwen3:8b" for the ollama
                backend (other backends take their own default).

---------------------------------------------------------------
 [[task.verify]]  and  [[task.session.verify]]   (zero or more)
---------------------------------------------------------------
 [[task.verify]] runs after ALL of a task's sessions complete.
 [[task.session.verify]] runs after its own session finishes.
 Exactly ONE of `command`, `brain`, or `prompt` is required.

 Key                Type           Notes
 id                 string         Stable id for restart_on references
 command            string  ONE-OF Shell command; exit 0 = PASS. (Works today.)
 brain              string  ONE-OF Local-LLM check (planned; not yet run in MVP).
 prompt             string  ONE-OF AI verifier prompt -- the check to run. (Works
                                   today.) NOT a backend name: unlike the
                                   session-level `agent`, this is instruction
                                   text. Runs using the owning session's resolved
                                   backend (its `agent`, e.g. claude/ollama) -- a
                                   verify step has no separate backend selector,
                                   so override only `model` here. The AI's final
                                   output must report a PASS/FAIL verdict
                                   (searched for anywhere in its output); no
                                   verdict found = FAIL. Put the fix-and-retry
                                   instructions in the prompt, e.g. "Run `<cmd>`;
                                   fix problems and re-run up to 3 times; if still
                                   failing, fail with an error. Do NOT commit and
                                   do NOT push."
 model              string         Model override for a `prompt` verifier
                                   (falls back to the owning session's resolved
                                   model when unset)
 arguments          array<string>  Extra flags for the prompt verifier (not yet
                                   wired up -- same as session-level `args`)
 budget_tokens      integer        Total-token cap for the prompt verifier (falls
                                   back to the task `budget_tokens`); exceeding
                                   it fails the verify step
 timeout_minutes    integer        Wall-clock timeout for the verifier (falls
                                   back to the task `timeout_minutes`)
 requires_approval  boolean        Pause for human sign-off (planned)
 restart_on         array<string>  Re-run this task when a referenced verify
                                   step fires. Grammar:
                                   "task/session/verify?on=pass|fail|both",
                                   with "task/*" and "task/session/*" wildcards.

 NOTE: today the runner executes `command` and `prompt` verifiers; `brain`
 (local-LLM) and `approval` (human) verifiers are still accepted by the schema
 but deferred, and stay pending.

---------------------------------------------------------------
 depends_on formats
---------------------------------------------------------------
   "session-id"              another session in the SAME task
   "task-name/session-id"    a session in another (earlier) task
   "task-name"               (task-level) the whole named task
   "run-id"                  ([[default]]) another run, gated on it being Done
   "run-id/task/session"     ([[default]]) a specific path in another run

===============================================================
              RECOMMENDED LAYOUT (per ticket/branch)
===============================================================

When turning tickets into tasks, prefer this shape. It keeps the
agent honest (verify actually fixes), keeps commits clean, and folds
a whole batch of branches into ONE review (not one per task).

Reviews group a BATCH, not a task. Given a list of tickets/tasks/runs,
default to a SINGLE shared review for the whole batch (at most a small
number -- e.g. split into two only when the work genuinely divides into
two independent streams). Do NOT create one review per task: a review
per branch is noise, buries the cross-branch conflict picture the
Guardian exists to surface, and is almost never what you want.

 1. PROMPT = the ticket, verbatim. Paste the ticket's own text
    (summary / acceptance criteria) into `prompt` as an near-exact
    copy -- only light tweaks (e.g. "your working directory is this
    worktree"). Do NOT paraphrase it into your own words; the ticket
    author's wording is the spec.

 2. WORK session: do the work, but DO NOT commit or push. Leave the
    tree dirty for the finalize step.

    When using agent = "claude-code", put the "do not commit / do
    not push" constraint in `system_prompt` (with
    system_prompt_position = "append") rather than inside `prompt`.
    A `system_prompt` is more authoritative: agents can
    de-prioritise or forget instructions buried in a long user-turn
    prompt, whereas system-prompt text is treated as a hard
    constraint for the entire session. Safety rails belong there.

      [[task.session]]
      id                     = "work"
      agent                  = "claude-code"
      cwd                    = "C:/Users/me/repo_worktrees/RAL-X"
      system_prompt          = "Do NOT commit and do NOT push under any circumstances."
      system_prompt_position = "append"
      prompt                 = "<< ticket text >>"

 3. PROMPT VERIFY steps: one per check (format, lint, test). Each is
    a `prompt` verifier whose text runs the command and fixes on
    failure, e.g.:
      prompt = "Run `cargo test --all-targets`. If it fails, fix the
                cause and re-run, up to 3 times. If still failing,
                fail with an error. Do NOT commit and do NOT push."
    (Use a `command` verifier instead only for a pure pass/fail gate
    with no auto-fix.)

 4. FINALIZE session (depends on the verifies passing): an AI session
    that stages only the intended SOURCE files -- deliberately NOT
    build artifacts or temp files -- commits on the worktree branch,
    and (if the repo has a remote) pushes it so the review can rebase
    it. Keep this an AI session, not `git add -A`, so junk never gets
    committed.

 4b. BRANCH STACKING (DEFAULT when this task continues prior work in
    the same repo): add `upstream = "<<task:prior-task-name>>"` to the
    WORK session. The scheduler rebases this branch onto that task's
    finalized branch tip before starting the runner -- so the agent
    always starts from the correct stack position without any manual
    rebase in the prompt. Use this whenever two tasks in the same repo
    are sequenced with depends_on; omit only for the first task in a
    chain or for tasks in different repos.

      [[task.session]]
      id                     = "work"
      upstream               = "<<task:prior-task-name>>"
      depends_on             = ["prior-task-name/finalize"]
      ...

 5. ONE review for the whole batch (the DEFAULT): give every branch's
    [[task.session.review]] the SAME id = "ralphus:new-review/<key>"
    so they ALL fold into a single Guardian -- even across separate
    .toml files / submissions. This is the norm for a list of tickets,
    not a special case. Reach for a second review only when the batch
    splits into two genuinely independent streams; otherwise keep it to
    one. Never emit one review per task -- that is the anti-pattern this
    layout exists to avoid.

 Ordering: branches that touch the SAME files should be sequenced
 with depends_on (so they stack cleanly); independent branches run in
 parallel.

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
  cwd      = "C:/Users/me/repo/.wt/feature-b"
  upstream = "<<task:setup>>"     # rebase feature-b onto setup's branch tip first
  prompt   = "Using {handoff:setup}, write a summary to report.md."

    [[task.session.review]]
    id   = "backend"
    base = "<<upstream>>"

    [[task.session.verify]]
    command = "test -f report.md"

-- 4. Recommended per-branch shape (agent verify + finalize) --

[[task]]
name = "ral-2"

  # work: do the work, leave it uncommitted.
  # system_prompt carries immutable constraints (no commit/push) as a
  # hard directive rather than burying them in the user-turn prompt.
  [[task.session]]
  id                     = "work"
  agent                  = "claude-code"
  cwd                    = "C:/Users/me/repo_worktrees/RAL-2"
  system_prompt          = "Do NOT commit and do NOT push under any circumstances."
  system_prompt_position = "append"
  prompt                 = "<< the RAL-2 ticket text, pasted verbatim >>"

    # one shared review for the whole batch (same key on every branch).
    # agent/model pick who resolves conflicts in THIS review (here: Claude,
    # instead of the default local ollama resolver).
    [[task.session.review]]
    id    = "ralphus:new-review/ral-batch"
    name  = "RAL batch"
    base  = "<<upstream>>"
    agent = "claude"

    # agent verifiers fix-and-retry, without committing
    [[task.session.verify]]
    id                     = "fmt"
    prompt                 = "Run cargo fmt --all; re-run up to 3x or fail. No commit/push."
    system_prompt          = "Do NOT commit, push, or make any file changes."
    system_prompt_position = "append"
    [[task.session.verify]]
    id                     = "test"
    prompt                 = "Run cargo test; fix and re-run up to 3x, else fail. No commit/push."
    system_prompt          = "Do NOT commit, push, or make any file changes."
    system_prompt_position = "append"

  # finalize: AI stages only source files and commits (depends on work).
  [[task.session]]
  id         = "finalize"
  cwd        = "C:/Users/me/repo_worktrees/RAL-2"
  depends_on = ["work"]
  prompt     = "Stage only source changes (no build/temp files), commit, push if a remote exists."

# A second ticket that stacks on ral-2. `upstream` rebases RAL-3's
# worktree branch onto ral-2's finalized tip before the agent starts.
[[task]]
name       = "ral-3"
depends_on = ["ral-2"]

  [[task.session]]
  id                     = "work"
  agent                  = "claude-code"
  cwd                    = "C:/Users/me/repo_worktrees/RAL-3"
  upstream               = "<<task:ral-2>>"
  system_prompt          = "Do NOT commit and do NOT push under any circumstances."
  system_prompt_position = "append"
  prompt                 = "<< the RAL-3 ticket text, pasted verbatim >>"

    [[task.session.review]]
    id    = "ralphus:new-review/ral-batch"   # same key folds into the same review
    name  = "RAL batch"
    base  = "<<upstream>>"
    agent = "claude"

Submit it:  ralphus submit tasks.toml
(Adds it as Pending -- schedulable now. Use --hold to stage as Queued.)
"""


def task_tutor() -> str:
    """Return the Task TOML tutorial text."""
    return TASK_TUTOR
