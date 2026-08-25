//! The Task TOML tutorial shown by `ralphus task show-tutor`, ported
//! verbatim from `cli/src/ralphus/tutor.py`. Keep close to ralphus's actual
//! schema (`core/src/schema.rs`, `core/src/validate.rs`) -- update this text
//! too when the schema changes. ASCII-only intentionally: a legacy Windows
//! console (cp1252) mangles box-drawing characters.

pub const TASK_TUTOR: &str = r#"
===============================================================
             ralphus  Task TOML -- Schema Reference
===============================================================

A Task TOML describes one or more interdependent cells (AI
prompts and/or deterministic shell commands) to run under the
ralphus runner. Every file must contain at least one [[task]].

Section headers use TOML array-of-tables syntax ([[...]]). Each
[[task]] starts a new task; each [[task.cell]] appends to the
most recent [[task]]; each [[task.cell.proof]] appends to the
most recent [[task.cell]]; each [[review]] is top-level; and so on.

Tip: validate before submitting -- `ralphus validate file.toml`
(runs offline, no daemon needed).

---------------------------------------------------------------
 [[default]]   (optional; only the first block is used)
---------------------------------------------------------------
 Key         Type           Notes
 depends_on  array<string>  Cross-squad gates: other squad-ids (or
                            "squad-id/task/cell" paths) that must be
                            Done before THIS whole submission starts.

---------------------------------------------------------------
 [[task]]   (required, one or more)
---------------------------------------------------------------
 Key          Type           Notes / example
 name         string  REQ    Unique task name, e.g. "build"
 project      string         Namespace label. REQUIRED, whenever a
                             placeholder cwd is used
                             (e.g. "<<ralphus:new-worktree/<branch>?upstream=<upstream>>>").
                             `project` MUST match a project from `ralphus project list`.
 root         string         The VCS root, if any. e.g. a git repository root.
 agent        string         Default agent for the task's cells
 model        string         Default model for the task's cells
 args           array<string>  Extra agent CLI flags for every cell
 budget_tokens  integer        Default total-token cap (in+out); cells/proofs
                               inherit it. Exceeding it fails the step.
 max_retries    integer        Auto-retry count on failure
 priority       integer        Initial Queue priority hint (lower = runs sooner).
                               Seeds this task's starting position in the Queue tab;
                               you reorder freely there afterwards, so it's only a
                               starting nudge, not a hard guarantee.
 timeout_minutes integer       Default wall-clock timeout (minutes) for cells
                               and proof steps; each may override its own.
 depends_on     array<string>  Tasks this task waits on (see formats below)
 environment    table<string,  Environment variables for every cell's spawned
                 string>       subprocess, e.g.
                               `environment = { FEATURE_FLAG = "1" }`.
                               Cells inherit these; a cell setting the
                               same key overrides it just for itself. Keys
                               must be valid identifiers
                               ([A-Za-z_][A-Za-z0-9_]*); values must be
                               strings. Same mechanism as (and can later be
                               changed/unset via) `POST /api/squads/{id}/tasks/
                               {ti}/env` -- see docs/daemon-api.md.
 no_commit_required boolean    Opt out of the automatic no-new-commits guard.
                               For a git-backed task (its `project` is
                               registered as git -- see "Project registry"
                               below), the daemon fails the task if none of
                               its cells produced a new commit since the
                               task started -- a deterministic check, not
                               reliant on the agent's self-report. Set this
                               for tasks that are legitimately expected to
                               produce no commits (e.g. a read-only
                               investigation). Defaults to false. Has no
                               effect on non-git-backed tasks, and never
                               applies to a manual `ralphus task set-status
                               done` override.

---------------------------------------------------------------
 [[task.cell]]   (zero or more per task)
---------------------------------------------------------------
 Exactly ONE of `prompt` or `command` is required.

 Key                     Type           Notes
 id                      string         Stable id for dependency references (no "/")
 name                    string         Human-readable display label shown in the board
                                        (card and detail pane). Falls back to `id` when
                                        unset. No structural meaning -- safe to rename.
 cwd                     string  REQ    RECOMMENDED: "<<ralphus:new-worktree/<branch>?upstream=<upstream>>>"
                                        -- a placeholder naming a branch to materialize.
                                        Requires the task's `project` field to name a
                                        project registered via `ralphus project git`
                                        (NOT embedded in `cwd` itself). The daemon
                                        resolves the `<<...>>` marker, creates (or
                                        reuses) a git worktree for <branch>, and
                                        rewrites `cwd` to that real path before the
                                        cell runs. The marker may be embedded in a
                                        longer value, e.g. "<<...>>/subdir". See
                                        "Project registry" below.
                                        The "?upstream=<upstream>" suffix names what
                                        branch <branch> should track -- i.e. what it is
                                        based on/compared against. Add it only when
                                        <branch> should track something (almost always the
                                        case for a review/feature branch). You rarely need
                                        to name a specific branch up front: use the
                                        reserved sentinel "?upstream=<<default>>" (the
                                        repo's default branch, recommended) and ralphus
                                        resolves it at run time, or
                                        "?upstream=<<current_branch>>" (whatever branch the
                                        project currently has checked out -- riskier, since
                                        it can silently change between runs). Name a literal
                                        branch -- a local one, e.g. "?upstream=main", or a
                                        remote-qualified one, e.g.
                                        "?upstream=origin/main" -- when it should track
                                        something other than the default (e.g. a specific
                                        remote branch it should resync with). A placeholder
                                        cwd with no "?upstream=" at all fails validation,
                                        so always include a suffix (sentinel or name). This
                                        value is what `derive_reviews` uses to find a
                                        review's base and what drives resync-on-reuse (fetch
                                        + rebase) for a remote-tracking branch -- it is a
                                        DIFFERENT thing from the cell's own `upstream` field
                                        below (that one rebases this cell's branch onto
                                        another task/cell's tip before the cell runs; this
                                        one only configures git tracking). A cell may need
                                        both.
                                        Examples:
                                          cwd = "ralphus:new-worktree/RAL-123-fix?upstream=<<default>>"
                                          cwd = "ralphus:new-worktree/RAL-123-fix?upstream=main"
                                          cwd = "ralphus:new-worktree/origin/feature/x?upstream=origin/blah"
                                          cwd = "<<ralphus:new-worktree/RAL-123-fix?upstream=main>>"
                                          cwd = "<<ralphus:new-worktree/origin/feature/x?upstream=origin/blah>>/package"
                                        ALTERNATIVE: an absolute path to an
                                        already-built git WORKTREE, if you built it
                                        yourself. ALWAYS use forward slashes (e.g.
                                        "C:/Users/me/repo/.wt/feat")!
 subprojects             array<string>  If your project lives inside a monorepo, declare
                                        which package subdirectories this cell
                                        focuses on, e.g.
                                        `subprojects = ["packages/foo", "libs/bar"]`.
                                        Ralphus will focus its edits in those
                                        directories. IMPORTANT: `cwd` should always
                                        point to the (mono)repo root WORKTREE. Omit
                                        `subprojects` (or leave empty) for
                                        single-project repos.
 prompt                  string  ONE-OF AI prompt text (launches an agent cell). May
                                        contain {handoff:<task-or-cell>} -- replaced
                                        at run time with the summaries of this cell's
                                        completed dependencies.
 command                 string  ONE-OF Deterministic shell command (no AI). Exit 0 = ok.
                                        Mutually exclusive with `prompt`. NOTE: command
                                        cells ignore `agent` and `model` entirely.
 role                    string         Optional role label
 agent                   string         Override the task agent for this cell
 model                   string         Override the task model for this cell
 system_prompt           string         Appended to the agent's system prompt before the
                                        cell starts. More authoritative than embedding
                                        the same text inside `prompt`: agents can
                                        de-prioritise or forget instructions buried in a
                                        long user-turn prompt, but system-prompt text is
                                        treated as a hard constraint throughout the cell.
                                        Use this for immutable safety rails such as
                                        "Do NOT commit and do NOT push." RESTRICTION:
                                        only valid with agent = "claude-code" (or when the
                                        task or global default resolves to "claude-code").
 system_prompt_position  string         Where `system_prompt` is injected. The only
                                        accepted value today is "append" (appended after
                                        the agent's built-in system prompt). Required
                                        whenever `system_prompt` is set.
 args                    array<string>  Per-cell agent flags (appended after task args)
 environment             table<string,  Environment variables for this cell's spawned
                         string>        subprocess, e.g.
                                        `environment = { FEATURE_FLAG = "1" }`.
                                        Merges with the owning task's
                                        `environment`, winning on a shared
                                        key. Same key/value rules as the
                                        task-level field above. Values support the
                                        same embedded `<<ralphus:new-worktree/...>>`
                                        expansion as `cwd`; repeat the exact marker
                                        to reuse that worktree's resolved path.
 budget_tokens           integer        Per-cell total-token cap (falls back to task)
 timeout_minutes         integer        Per-cell wall-clock timeout (falls back to task)
 priority                integer        Initial Queue priority hint (lower = runs sooner);
                                        seeds this cell's starting Queue position.
 depends_on              array<string>  Cells that must finish first (see formats below)
 upstream               string         Branch-chaining sentinel (DIFFERENT from
                                        `cwd`'s "?upstream=" query above -- that one
                                        sets git tracking; this one triggers a rebase).
                                        Set to "<<task:task-name>>" or
                                        "<<task:task-name/cell-id>>" and
                                        the scheduler rebases THIS cell's
                                        branch onto that dependency's current
                                        branch tip immediately before starting
                                        the runner. Use whenever this task picks
                                        up where a prior task left off in the
                                        same repo. Cross-repo upstreams are
                                        silently skipped with a warning.
                                        THIS IS THE DEFAULT RECOMMENDATION for
                                        sequential tasks in the same repo --
                                        prefer it over rebasing manually in the
                                        prompt or finalize cell.

 Agent selection:
   For `agent = "{<...>}"`
   Run `ralphus agent list` for the full set of supported agents and, for
   each, either its fixed set of allowed models or "<any model>".

   Custom agent profiles live in `.ralphus.toml`, not in task TOML:
     [agent.profiles.openrouter-deepseek]
     backend = "codex"
     executable = "codex-openrouter"
     [agent.profiles.openrouter-deepseek.env]
     OPENROUTER_API_KEY = { from_env = "OPENROUTER_API_KEY" }

   Then a cell uses it with the normal existing field:
     agent = "openrouter-deepseek"

   IMPORTANT:
   - profile secrets stay out of task TOML and out of HTTP request/response
     bodies; they are resolved server-side from `.ralphus.toml` plus the
     daemon's own OS environment.
   - if a cell resolves to a custom agent profile, do NOT also set `model`.
     Current v1 rule: "if you're using a custom agent profile, you can't
     also set `model`".
   - `backend = "raw"` is the explicit generic-executable backend. It MUST
     be used through a profile that also sets `executable`; the old implicit
     "unknown agent name = generic harness" fallback is gone.

   PLACEHOLDER NOTATION used below and throughout the EXAMPLES:
     <<...>>   a LITERAL placeholder to keep as-is -- real ralphus syntax
               the daemon itself parses at submit/run time, e.g.
               "<<task:task-name>>" (see `upstream` above).
     {<...>}   a value YOU must fill in before submitting -- never leave
               this literal text in a real TOML file.

   IMPORTANT: every `agent = "..."` below (task, cell, AND the review's
   resolver `agent`) is a {<...>} PLACEHOLDER --
     agent = "{<insert recommended agent here>}"  # claude-code, codex-cli, claude, ollama, etc
   -- decide the real value per submission (from the request, ticket, or
   project convention). Do NOT blindly copy a literal agent value (e.g.
   "claude" or "claude-code") out of this tutorial into an unrelated
   submission just because an example happened to show it.

---------------------------------------------------------------
 [[review]]   (zero or more per file, top-level)
---------------------------------------------------------------
 Declares a Guardian code review. Cells opt in by setting
 `review = "<id>"` in their [[task.cell]] block. Branch order
 follows the task/cell dependency order.

 The review's base branch is always resolved from the worktree's
 upstream tracking branch at submit time (a hard error if the
 branch has no upstream). Set the upstream with:
   git branch --set-upstream-to=main <your-branch>

 Two ways branches fold into a review:
   * By PROJECT (default): cells with a plain `id` are grouped
     by the shared git dir of their worktree -- all worktrees of
     one repo fold into one review; cells spanning N projects
     make N reviews with names disambiguated automatically.
   * By LINK KEY (recommended for a planned batch): set
     id = "ralphus:new-review/<key>". Every cell naming the
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
     Default recommendation: unless the user explicitly asks for a
     different split, make ONE Guardian review per `ralphus submit`
     call. Multiple reviews are fine when intentional; one review
     per submit is simply the safe default.

 Key    Type    Notes
 id     string  Human-readable review id (also the default label),
                OR a link placeholder "ralphus:new-review/<key>"
                whose <key> is letters/digits/-/_/. Cells
                reference this review by setting `review = "<id>"`.
 name   string  GUI label; falls back to `id` when unset. For a
                link id, set `name` so the review has a readable
                label (not the raw URI).
 agent  string  Backend that RESOLVES this review's merge conflicts
                (and applies reviewer feedback), e.g. "claude" or
                "ollama". This is the conflict-fixer, NOT the
                cell that produced the branch. Unset falls back
                to RALPHUS_RESOLVER_AGENT env, then
                .ralphus.toml's [review].default_resolver_agent,
                then "ollama".
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
 [[task.proof]]  and  [[task.cell.proof]]   (zero or more)
---------------------------------------------------------------
 [[task.proof]] runs after ALL of a task's cells complete.
 [[task.cell.proof]] runs after its own cell finishes.
 Exactly ONE of `command`, `brain`, or `prompt` is required.

 Key                Type           Notes
 id                 string         Stable id for restart_on references
 command            string  ONE-OF Shell command; exit 0 = PASS. (Works today.)
 brain              string  ONE-OF Local-LLM check (planned; not yet run in MVP).
 prompt             string  ONE-OF AI proof prompt -- the check to run. (Works
                                   today.) NOT a backend name: unlike the
                                   cell-level `agent`, this is instruction
                                   text. Runs using the owning cell's resolved
                                   backend (its `agent`, e.g. claude/ollama) -- a
                                   proof step has no separate backend selector,
                                   so override only `model` here. The AI's final
                                   output must report a PASS/FAIL verdict
                                   (searched for anywhere in its output); no
                                   verdict found = FAIL. Put the fix-and-retry
                                   instructions in the prompt, e.g. "Run `{<cmd>}`;
                                   fix problems and re-run up to 3 times; if still
                                   failing, fail with an error. Do NOT commit and
                                   do NOT push."
 model              string         Model override for a `prompt` proof step
                                   (falls back to the owning cell's resolved
                                   model when unset)
 arguments          array<string>  Extra flags for the prompt proof step (not yet
                                   wired up -- same as cell-level `args`)
 budget_tokens      integer        Total-token cap for the prompt proof step (falls
                                   back to the task `budget_tokens`); exceeding
                                   it fails the proof step
 timeout_minutes    integer        Wall-clock timeout for the proof step (falls
                                   back to the task `timeout_minutes`)
 requires_approval  boolean        Pause for human sign-off (planned)
 restart_on         array<string>  Re-run this task when a referenced proof
                                   step fires. Grammar:
                                   "task/cell/proof?on=pass|fail|both",
                                   with "task/*" and "task/cell/*" wildcards.
 environment        table<string,  Environment variables for THIS ONE proof
                    string>        step's spawned subprocess, e.g.
                                   `environment = { RUST_LOG = "debug" }`.
                                   The narrowest layer: merges with (and wins
                                   over on a shared key) the owning cell's
                                   and task's `environment`. Per-step, so two
                                   [[task.proof]] blocks can set the same key
                                   to different values without colliding.

 NOTE: today the runner executes `command` and `prompt` proof steps; `brain`
 (local-LLM) and `approval` (human) proof steps are still accepted by the schema
 but deferred, and stay pending.

---------------------------------------------------------------
 Project registry + placeholder cwd
---------------------------------------------------------------
 A cell `cwd` may be the placeholder
 "<<ralphus:new-worktree/<branch>?upstream=<upstream>>>" instead of a real path.
 Register the project once, up front:

   ralphus project git --path C:/Users/me/repo --name my-project --description "Backend API service"

 Then reference it by name on the TASK (not embedded in `cwd`) -- no need to
 know or build the worktree path yourself:

   [[task]]
   name    = "add-feature"
   project = "my-project"          # REQUIRED: must match a registered name

     [[task.cell]]
     cwd    = "<<ralphus:new-worktree/RAL-123-add_feature?upstream=main>>"
     prompt = "..."

 The daemon resolves "my-project" (the task's `project` field) against its
 registry, creates (or reuses, across restarts) a git worktree for the
 branch named inside the `<<ralphus:new-worktree/...>>` marker, and rewrites cwd to that real
 path before the cell runs. Both forms of `cwd` are valid -- a plain
 absolute path (you already built the worktree yourself) or this placeholder
 (the daemon builds it for you). A near-miss project name (e.g. from
 speech-to-text) still resolves via fuzzy/description matching, but prefer
 exact names.

 The trailing "?upstream=<upstream>" tells ralphus what <branch> tracks (its
 base/compare target) rather than guessing from HEAD. It is only something
 you need to pin down explicitly when that target *can't* be inferred; for
 the common case -- a fresh feature branch based on the repo's default
 branch -- use the sentinel "?upstream=<<default>>" instead of hardcoding a
 name:

   [[task.cell]]
   cwd    = "ralphus:new-worktree/RAL-124-other_feature?upstream=<<default>>"
   prompt = "..."

 ralphus resolves "<<default>>" to the project's default branch at run time.
 The alternative sentinel, "?upstream=<<current_branch>>", tracks whatever
 branch the project currently has checked out -- use it with care, since that
 can silently change between runs. Give a literal name ("?upstream=main", or
 a remote-qualified "?upstream=origin/<branch>") only when <branch> should
 track something other than the default -- e.g. a remote branch you want it
 to resync with. A placeholder cwd with no "?upstream=" at all fails
 validation, so always include a suffix (a sentinel or a literal name).

---------------------------------------------------------------
 depends_on formats
---------------------------------------------------------------
   "cell-id"                 another cell in the SAME task
   "task-name/cell-id"       a cell in another (earlier) task
   "task-name"               (task-level) the whole named task
   "squad-id"                ([[default]]) another squad, gated on it being Done
   "squad-id/task/cell"      ([[default]]) a specific path in another squad

===============================================================
              RECOMMENDED LAYOUT (per ticket/branch)
===============================================================

When turning tickets into tasks, prefer this shape. It keeps the
agent honest (proof actually fixes), keeps commits clean, and folds
a whole batch of branches into ONE review (not one per task).

Reviews group a BATCH, not a task. Given a list of tickets/tasks/squads,
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

 2. WORK cell: do the work, but DO NOT commit or push. Leave the
    tree dirty for the finalize step.

    When using agent = "{<...>}", put the "do not commit / do
    not push" constraint in `system_prompt` (with
    system_prompt_position = "append") rather than inside `prompt`.
    A `system_prompt` is more authoritative: agents can
    de-prioritise or forget instructions buried in a long user-turn
    prompt, whereas system-prompt text is treated as a hard
    constraint for the entire cell. Safety rails belong there.

      # cwd's branch materializes under the task's `project` (below).
      [[task.cell]]
      id                     = "work"
      agent                  = "{<insert recommended agent here>}"  # see "Agent selection" above
      cwd                    = "<<ralphus:new-worktree/RAL-X?upstream=main>>"
      system_prompt          = "Do NOT commit and do NOT push under any circumstances."
      system_prompt_position = "append"
      prompt                 = "{<ticket text>}"

 3. PROMPT PROOF steps: one per check (format, lint, test). Each is
    a `prompt` proof step whose text runs the command and fixes on
    failure, e.g.:
      prompt = "Run `cargo test --all-targets`. If it fails, fix the
                cause and re-run, up to 3 times. If still failing,
                fail with an error. Do NOT commit and do NOT push."
    (Use a `command` proof step instead only for a pure pass/fail gate
    with no auto-fix.)

 4. FINALIZE cell (depends on the proof steps passing): an AI cell
    that stages only the intended SOURCE files -- deliberately NOT
    build artifacts or temp files -- commits on the worktree branch,
    and (if the repo has a remote) pushes it so the review can rebase
    it. Keep this an AI cell, not `git add -A`, so junk never gets
    committed.

    Put "do not run formatters, linters, or tests" in `system_prompt`
    (system_prompt_position = "append"), not in `prompt`. The proof
    steps already ran those checks -- finalize re-running them (and
    possibly "fixing" something) risks producing a diff that never
    went through proof, and can distract the agent from its one job.
    A `system_prompt` is a hard constraint, so it reliably keeps
    finalize to exactly: stage the intended source files, commit, and
    push.

 4b. BRANCH STACKING (DEFAULT when this task continues prior work in
    the same repo): add `upstream = "<<task:prior-task-name>>"` to the
    WORK cell. The scheduler rebases this branch onto that task's
    finalized branch tip before starting the runner -- so the agent
    always starts from the correct stack position without any manual
    rebase in the prompt. Use this whenever two tasks in the same repo
    are sequenced with depends_on; omit only for the first task in a
    chain or for tasks in different repos.

      [[task.cell]]
      id                     = "work"
      upstream               = "<<task:prior-task-name>>"
      depends_on             = ["prior-task-name/finalize"]
      ...

 5. ONE review for the whole batch (the DEFAULT unless the user says
    otherwise): add a top-level [[review]] block with id =
    "ralphus:new-review/<key>", then set review =
    "ralphus:new-review/<key>" on every [[task.cell]] whose cwd is a
    git worktree. Every cell naming the same <key> attaches to ONE
    shared Guardian -- but the <key> only groups WITHIN a single
    submission. The prescriptive default is one Guardian review per
    `ralphus submit` call. Reach for a second review only when the batch
    splits into two genuinely independent streams; otherwise keep it to
    one. Never emit one review per task -- that is the anti-pattern this
    layout exists to avoid.

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

  [[task.cell]]
  cwd    = "<<ralphus:new-worktree/hello?upstream=main>>"
  prompt = "Print 'Hello, World!' to a new file hello.txt."

-- 2. A local (Ollama) deterministic check --------------------

[[task]]
name    = "check"
project = "my-project"

  [[task.cell]]
  cwd     = "<<ralphus:new-worktree/check?upstream=main>>"
  agent   = "{<...put your recommended agent here>}"
  model   = "qwen3:8b"
  command = "cargo test"          # command ignores agent/model anyway

-- 3. Two tasks, a handoff, and a per-project review ----------

[[task]]
name    = "setup"
project = "my-project"

  [[task.cell]]
  id     = "init"
  cwd    = "<<ralphus:new-worktree/feature-a?upstream=main>>"
  prompt = "Create data.json with {\"version\": 1}."
  review = "backend"              # opt this worktree branch into the review

    [[task.cell.proof]]
    command = "test -f data.json"

[[task]]
name       = "report"
project    = "my-project"
depends_on = ["setup"]            # waits for all of setup's cells + proof steps

  [[task.cell]]
  cwd      = "<<ralphus:new-worktree/feature-b?upstream=main>>"
  upstream = "<<task:setup>>"     # rebase feature-b onto setup's branch tip first
  prompt   = "Using {handoff:setup}, write a summary to report.md."
  review   = "backend"

    [[task.cell.proof]]
    command = "test -f report.md"

# Top-level review declaration (base is always the worktree's upstream tracking branch).
[[review]]
id = "backend"

-- 4. Recommended per-branch shape (agent proof + finalize) --

# One shared review for the whole batch. `agent` here is the CONFLICT-RESOLVER
# backend (see the [[review]] table above), a separate decision from the
# cell `agent` below -- pick both deliberately, per submission.
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
project = "my-project"            # both cells' worktree materializes under this

  # work: do the work, leave it uncommitted.
  # system_prompt carries immutable constraints (no commit/push) as a
  # hard directive rather than burying them in the user-turn prompt.
  [[task.cell]]
  id                     = "work"
  agent                  = "claude-code"  # required for system_prompt (see field reference above)
  cwd                    = "<<ralphus:new-worktree/ral-2?upstream=main>>"
  review                 = "ralphus:new-review/ral-batch"
  system_prompt          = "Do NOT commit and do NOT push under any circumstances."
  system_prompt_position = "append"
  prompt                 = "{<the RAL-2 ticket text, pasted verbatim>}"

    # agent proof steps fix-and-retry, without committing. NOTE: unlike the
    # work cell's system_prompt, this must NOT also forbid file changes --
    # fixing a lint or a failing test requires editing files, which would
    # contradict the fix-and-retry instruction in `prompt` above.
    [[task.cell.proof]]
    id                     = "fmt"
    prompt                 = "Run cargo fmt --all; re-run up to 3x or fail. No commit/push."
    system_prompt          = "Do NOT commit and do NOT push under any circumstances."
    system_prompt_position = "append"
    [[task.cell.proof]]
    id                     = "test"
    prompt                 = "Run cargo test; fix and re-run up to 3x, else fail. No commit/push."
    system_prompt          = "Do NOT commit and do NOT push under any circumstances."
    system_prompt_position = "append"

  # finalize: AI stages only source files and commits (depends on work).
  # Same placeholder string as "work" -- the daemon materializes it once and
  # reuses the identical worktree for both cells. system_prompt keeps it
  # to exactly commit + push -- no re-running formatters/linters/tests that
  # proof already handled.
  [[task.cell]]
  id                     = "finalize"
  agent                  = "claude-code"  # required for system_prompt (see field reference above)
  cwd                    = "<<ralphus:new-worktree/ral-2?upstream=main>>"
  depends_on             = ["work"]
  system_prompt          = "Do NOT run formatters, linters, or tests. Just stage, commit, and push."
  system_prompt_position = "append"
  prompt                 = """Stage only source changes (no build/temp files), commit, \
                             push if a remote exists."""

# A second ticket that stacks on ral-2. `upstream` rebases RAL-3's
# worktree branch onto ral-2's finalized tip before the agent starts.
[[task]]
name       = "ral-3"
project    = "my-project"
depends_on = ["ral-2"]

  [[task.cell]]
  id                     = "work"
  agent                  = "claude-code"  # required for system_prompt (see field reference above)
  cwd                    = "<<ralphus:new-worktree/ral-3?upstream=main>>"
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

  [[task.cell]]
  cwd    = "<<ralphus:new-worktree/RAL-999-add_widget?upstream=main>>"
  # Repeating the same marker in an environment value reuses this worktree.
  environment = { WIDGET_WORKTREE = "<<ralphus:new-worktree/RAL-999-add_widget?upstream=main>>" }
  prompt = "Add a widget module."

Submit it:  ralphus submit tasks.toml
(Adds the squad as Pending -- schedulable now. Use --hold to stage as Queued.)

Multiple files, one submission: `ralphus submit` accepts more than one
file --
  ralphus submit ral-1.toml ral-2.toml ral-3.toml
combines them into ONE submission before sending it to the daemon, so a
`ralphus:new-review/<key>` shared across those files' [[review]] blocks
folds into ONE guardian. Submitting the files one at a time instead
(three separate `ralphus submit` calls) mints three separate guardians.
Unless the user explicitly asks for a different split, recommend the
former shape: one submit call, one Guardian review.
"#;

/// Returns the Task TOML tutorial text.
#[must_use]
pub fn task_tutor() -> &'static str {
    TASK_TUTOR
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tutor_is_ascii_only() {
        assert!(
            TASK_TUTOR.is_ascii(),
            "tutor text must stay ASCII-only for legacy Windows consoles"
        );
    }

    #[test]
    fn task_tutor_returns_the_constant() {
        assert_eq!(task_tutor(), TASK_TUTOR);
    }

    #[test]
    fn tutor_mentions_core_sections() {
        assert!(TASK_TUTOR.contains("[[task]]"));
        assert!(TASK_TUTOR.contains("[[task.cell]]"));
        assert!(TASK_TUTOR.contains("[[review]]"));
    }

    #[test]
    fn tutor_uses_wrapped_worktree_placeholders_for_cwd_examples() {
        assert!(TASK_TUTOR.contains("cwd    = \"<<ralphus:new-worktree/hello?upstream=main>>\""));
        assert!(
            !TASK_TUTOR
                .lines()
                .any(|line| line.contains("cwd") && line.contains("= \"ralphus:new-worktree/")),
            "cwd examples must teach the wrapped <<...>> expansion syntax"
        );
    }

    #[test]
    fn tutor_shows_an_environment_value_reusing_the_cwd_worktree() {
        assert!(TASK_TUTOR.contains(
            "environment = { WIDGET_WORKTREE = \"<<ralphus:new-worktree/RAL-999-add_widget?upstream=main>>\" }"
        ));
        assert!(
            TASK_TUTOR.contains(
                "Repeating the same marker in an environment value reuses this worktree."
            )
        );
    }
}
