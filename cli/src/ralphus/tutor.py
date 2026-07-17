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
most recent [[task]]; each [[task.session.verify]] appends to the
most recent [[task.session]]; each [[review]] is top-level; and so on.

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
 project      string         Namespace label. REQUIRED, whenever a
                             placeholder cwd is used
                             (e.g. "ralphus:new-worktree/<branch>").
                             `project` MUST match a project from `ralphus project list`.
 root         string         The VCS root, if any. e.g. a git repository root.
 agent        string         Default agent for the task's sessions
 model        string         Default model for the task's sessions
 args           array<string>  Extra agent CLI flags for every session
 budget_tokens  integer        Default total-token cap (in+out); sessions/verifies
                               inherit it. Exceeding it fails the step.
 max_retries    integer        Auto-retry count on failure
 priority       integer        Initial Queue priority hint (lower = runs sooner).
                               Seeds this task's starting position in the Queue tab;
                               you reorder freely there afterwards, so it's only a
                               starting nudge, not a hard guarantee.
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
 cwd                     string  REQ    RECOMMENDED: "ralphus:new-worktree/<branch>" -- a
                                        placeholder naming a branch to materialize.
                                        Requires the task's `project` field to name a
                                        project registered via `ralphus project git`
                                        (NOT embedded in `cwd` itself). The daemon
                                        resolves it, creates (or reuses) a git worktree
                                        for <branch>, and rewrites `cwd` to that real
                                        path before the session runs. See "Project
                                        registry" below.
                                        ALTERNATIVE: an absolute path to an
                                        already-built git WORKTREE, if you built it
                                        yourself. ALWAYS use forward slashes (e.g.
                                        "C:/Users/me/repo/.wt/feat")!
 subprojects             array<string>  If your project lives inside a monorepo, declare
                                        which package subdirectories this session
                                        focuses on, e.g.
                                        `subprojects = ["packages/foo", "libs/bar"]`.
                                        Ralphus will focus its edits in those
                                        directories. IMPORTANT: `cwd` should always
                                        point to the (mono)repo root WORKTREE. Omit
                                        `subprojects` (or leave empty) for
                                        single-project repos.
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
 priority                integer        Initial Queue priority hint (lower = runs sooner);
                                        seeds this session's starting Queue position.
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
   For `agent = "{<...>}"`
   Run `ralphus agent list` for the full set of supported agents and, for
   each, either its fixed set of allowed models or "<any model>".

   PLACEHOLDER NOTATION used below and throughout the EXAMPLES:
     <<...>>   a LITERAL placeholder to keep as-is -- real ralphus syntax
               the daemon itself parses at submit/run time, e.g.
               "<<task:task-name>>" (see `upstream` above).
     {<...>}   a value YOU must fill in before submitting -- never leave
               this literal text in a real TOML file.

   IMPORTANT: every `agent = "..."` below (task, session, AND the review's
   resolver `agent`) is a {<...>} PLACEHOLDER --
     agent = "{<insert recommended agent here>}"  # claude-code, codex-cli, claude, ollama, etc
   -- decide the real value per submission (from the request, ticket, or
   project convention). Do NOT blindly copy a literal agent value (e.g.
   "claude" or "claude-code") out of this tutorial into an unrelated
   submission just because an example happened to show it.

---------------------------------------------------------------
 [[review]]   (zero or more per file, top-level)
---------------------------------------------------------------
 Declares a Guardian code review. Sessions opt in by setting
 `review = "<id>"` in their [[task.session]] block. Branch order
 follows the task/session dependency order.

 The review's base branch is always resolved from the worktree's
 upstream tracking branch at submit time (a hard error if the
 branch has no upstream). Set the upstream with:
   git branch --set-upstream-to=main <your-branch>

 Two ways branches fold into a review:
   * By PROJECT (default): sessions with a plain `id` are grouped
     by the shared git dir of their worktree -- all worktrees of
     one repo fold into one review; sessions spanning N projects
     make N reviews with names disambiguated automatically.
   * By LINK KEY (recommended for a planned batch): set
     id = "ralphus:new-review/<key>". Every session naming the
     same <key> attaches to ONE shared review -- but ONLY within
     the SAME submission (one `ralphus submit` call). The key is a
     submission-LOCAL placeholder, not a stable cross-submission
     link: two separate `ralphus submit` invocations that happen to
     reuse the same <key> mint TWO independent guardians, not one.
     To fold several TOML files into ONE review, pass them all to a
     SINGLE `ralphus submit` call --
       ralphus submit file-a.toml file-b.toml file-c.toml
     -- which combines them into one submission before sending it
     to the daemon.

 Key    Type    Notes
 id     string  Human-readable review id (also the default label),
                OR a link placeholder "ralphus:new-review/<key>"
                whose <key> is letters/digits/-/_/. Sessions
                reference this review by setting `review = "<id>"`.
 name   string  GUI label; falls back to `id` when unset. For a
                link id, set `name` so the review has a readable
                label (not the raw URI).
 agent  string  Backend that RESOLVES this review's merge conflicts
                (and applies reviewer feedback), e.g. "claude" or
                "ollama". This is the conflict-fixer, NOT the
                session that produced the branch. Unset falls back
                to RALPHUS_RESOLVER_AGENT env, then "ollama".
 model  string  Model the resolver `agent` runs, e.g. "qwen3:8b".
                Unset falls back to RALPHUS_RESOLVER_MODEL, then
                "qwen3:8b" for the ollama backend.

 [[review.action]]  (zero or more per [[review]])
 User-declared labelled buttons shown in the review pane.
 Exactly ONE of `prompt` or `command` is required per entry.

 Key     Type    Notes
 label   string  REQUIRED. Text shown on the UI button.
 command string  ONE-OF Verbatim shell command run in a terminal.
 prompt  string  ONE-OF Hint text forwarded to the resolver LLM
                 to expand into a runnable command before running.

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
                                   instructions in the prompt, e.g. "Run `{<cmd>}`;
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
 Project registry + placeholder cwd
---------------------------------------------------------------
 A session `cwd` may be the placeholder "ralphus:new-worktree/<branch>"
 instead of a real path. Register the project once, up front:

   ralphus project git --path C:/Users/me/repo --name my-project --description "Backend API service"

 Then reference it by name on the TASK (not embedded in `cwd`) -- no need to
 know or build the worktree path yourself:

   [[task]]
   name    = "add-feature"
   project = "my-project"          # REQUIRED: must match a registered name

     [[task.session]]
     cwd    = "ralphus:new-worktree/RAL-123-add_feature"
     prompt = "..."

 The daemon resolves "my-project" (the task's `project` field) against its
 registry, creates (or reuses, across restarts) a git worktree for the
 branch named after "ralphus:new-worktree/", and rewrites cwd to that real
 path before the session runs. Both forms of `cwd` are valid -- a plain
 absolute path (you already built the worktree yourself) or this placeholder
 (the daemon builds it for you). A near-miss project name (e.g. from
 speech-to-text) still resolves via fuzzy/description matching, but prefer
 exact names.

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

    When using agent = "{<...>}", put the "do not commit / do
    not push" constraint in `system_prompt` (with
    system_prompt_position = "append") rather than inside `prompt`.
    A `system_prompt` is more authoritative: agents can
    de-prioritise or forget instructions buried in a long user-turn
    prompt, whereas system-prompt text is treated as a hard
    constraint for the entire session. Safety rails belong there.

      # cwd's branch materializes under the task's `project` (below).
      [[task.session]]
      id                     = "work"
      agent                  = "{<insert recommended agent here>}"  # see "Agent selection" above
      cwd                    = "ralphus:new-worktree/RAL-X"
      system_prompt          = "Do NOT commit and do NOT push under any circumstances."
      system_prompt_position = "append"
      prompt                 = "{<ticket text>}"

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

 5. ONE review for the whole batch (the DEFAULT): add a top-level
    [[review]] block with id = "ralphus:new-review/<key>", then set
    review = "ralphus:new-review/<key>" on every [[task.session]] whose
    cwd is a git worktree. Every session naming the same <key> attaches
    to ONE shared Guardian -- but the <key> only groups WITHIN a single
    submission. This is the norm for a list of tickets, not a special
    case. Reach for a second review only when the batch splits into two
    genuinely independent streams; otherwise keep it to one. Never emit
    one review per task -- that is the anti-pattern this layout exists
    to avoid.

    IMPORTANT: if the batch spans MULTIPLE .toml files (one per
    ticket/branch is the usual layout), every file that shares the same
    <key> MUST be included in the SAME `ralphus submit` call:
      ralphus submit ral-1.toml ral-2.toml ral-3.toml
    Submitting them one file at a time (three separate `ralphus submit`
    calls) mints THREE separate guardians, even though every file
    declares the identical <key> -- the link key does not persist
    across submissions. Every top-level [[review]] block must also be
    repeated once per file (not just once across the whole batch) so
    each file's own review-derivation pass has something to key off of.

 Ordering: branches that touch the SAME files should be sequenced
 with depends_on (so they stack cleanly); independent branches run in
 parallel.

===============================================================
                          EXAMPLES
===============================================================

-- 1. Hello, world --------------------------------------------

[[task]]
name    = "hello"
project = "my-project"            # REQUIRED: must match a registered name

  [[task.session]]
  cwd    = "ralphus:new-worktree/hello"
  prompt = "Print 'Hello, World!' to a new file hello.txt."

-- 2. A local (Ollama) deterministic check --------------------

[[task]]
name    = "check"
project = "my-project"

  [[task.session]]
  cwd     = "ralphus:new-worktree/check"
  agent   = "{<...put your recommended agent here>}"
  model   = "qwen3:8b"
  command = "cargo test"          # command ignores agent/model anyway

-- 3. Two tasks, a handoff, and a per-project review ----------

[[task]]
name    = "setup"
project = "my-project"

  [[task.session]]
  id     = "init"
  cwd    = "ralphus:new-worktree/feature-a"
  prompt = "Create data.json with {\"version\": 1}."
  review = "backend"              # opt this worktree branch into the review

    [[task.session.verify]]
    command = "test -f data.json"

[[task]]
name       = "report"
project    = "my-project"
depends_on = ["setup"]            # waits for all of setup's sessions + verifiers

  [[task.session]]
  cwd      = "ralphus:new-worktree/feature-b"
  upstream = "<<task:setup>>"     # rebase feature-b onto setup's branch tip first
  prompt   = "Using {handoff:setup}, write a summary to report.md."
  review   = "backend"

    [[task.session.verify]]
    command = "test -f report.md"

# Top-level review declaration (base is always the worktree's upstream tracking branch).
[[review]]
id = "backend"

-- 4. Recommended per-branch shape (agent verify + finalize) --

# One shared review for the whole batch. `agent` here is the CONFLICT-RESOLVER
# backend (see the [[review]] table above), a separate decision from the
# session `agent` below -- pick both deliberately, per submission.
# The base branch is always resolved from each worktree's upstream tracking branch.
[[review]]
id    = "ralphus:new-review/ral-batch"
name  = "RAL batch"
agent = "{<insert recommended agent here>}"  # claude-code, codex-cli, claude, ollama, etc

# Optional: user-declared test buttons shown in the review pane.
[[review.action]]
label   = "Run tests"
command = "cargo test --all-targets"

[[review.action]]
label  = "Frontend smoke test"
prompt = "Open localhost:3000 and click through the main workflows"

[[task]]
name    = "ral-2"
project = "my-project"            # both sessions' worktree materializes under this

  # work: do the work, leave it uncommitted.
  # system_prompt carries immutable constraints (no commit/push) as a
  # hard directive rather than burying them in the user-turn prompt.
  [[task.session]]
  id                     = "work"
  agent                  = "claude-code"  # required for system_prompt (see field reference above)
  cwd                    = "ralphus:new-worktree/ral-2"
  review                 = "ralphus:new-review/ral-batch"
  system_prompt          = "Do NOT commit and do NOT push under any circumstances."
  system_prompt_position = "append"
  prompt                 = "{<the RAL-2 ticket text, pasted verbatim>}"

    # agent verifiers fix-and-retry, without committing. NOTE: unlike the
    # work session's system_prompt, this must NOT also forbid file changes --
    # fixing a lint or a failing test requires editing files, which would
    # contradict the fix-and-retry instruction in `prompt` above.
    [[task.session.verify]]
    id                     = "fmt"
    prompt                 = "Run cargo fmt --all; re-run up to 3x or fail. No commit/push."
    system_prompt          = "Do NOT commit and do NOT push under any circumstances."
    system_prompt_position = "append"
    [[task.session.verify]]
    id                     = "test"
    prompt                 = "Run cargo test; fix and re-run up to 3x, else fail. No commit/push."
    system_prompt          = "Do NOT commit and do NOT push under any circumstances."
    system_prompt_position = "append"

  # finalize: AI stages only source files and commits (depends on work).
  # Same placeholder string as "work" -- the daemon materializes it once and
  # reuses the identical worktree for both sessions.
  [[task.session]]
  id         = "finalize"
  cwd        = "ralphus:new-worktree/ral-2"
  depends_on = ["work"]
  prompt     = "Stage only source changes (no build/temp files), commit, push if a remote exists."

# A second ticket that stacks on ral-2. `upstream` rebases RAL-3's
# worktree branch onto ral-2's finalized tip before the agent starts.
[[task]]
name       = "ral-3"
project    = "my-project"
depends_on = ["ral-2"]

  [[task.session]]
  id                     = "work"
  agent                  = "claude-code"  # required for system_prompt (see field reference above)
  cwd                    = "ralphus:new-worktree/ral-3"
  upstream               = "<<task:ral-2>>"
  review                 = "ralphus:new-review/ral-batch"  # same key folds into same review
  system_prompt          = "Do NOT commit and do NOT push under any circumstances."
  system_prompt_position = "append"
  prompt                 = "{<the RAL-3 ticket text, pasted verbatim>}"

-- 5. Placeholder cwd (daemon builds the git worktree) --------

# "my-project" must already be registered: ralphus project git ...
[[task]]
name    = "add-widget"
project = "my-project"

  [[task.session]]
  cwd    = "ralphus:new-worktree/RAL-999-add_widget"
  prompt = "Add a widget module."

Submit it:  ralphus submit tasks.toml
(Adds it as Pending -- schedulable now. Use --hold to stage as Queued.)

Multiple files, one submission: `ralphus submit` accepts more than one
file --
  ralphus submit ral-1.toml ral-2.toml ral-3.toml
combines them into ONE submission before sending it to the daemon, so a
`ralphus:new-review/<key>` shared across those files' [[review]] blocks
folds into ONE guardian. Submitting the files one at a time instead
(three separate `ralphus submit` calls) mints three separate guardians.
"""


def task_tutor() -> str:
    """Return the Task TOML tutorial text."""
    return TASK_TUTOR
