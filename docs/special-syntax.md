# Special syntax and markers — words that trigger ralphus behavior

Some ordinary-looking language in a ralphus task file, cell reply, or review
conversation acts like a command. This guide is the canonical list of those
triggers: what each one looks like, where it applies, what it does, and where
it can surprise you.

The triggers fall into two categories, kept separate throughout:

1. **Machine-parsed syntax** — words and markers ralphus code itself
   validates, resolves, or parses. These behave the same no matter who types
   them, because a parser reads them.
2. **Agent-facing conventions** — phrases that influence behavior only
   because they map naturally onto a CLI command or an agent instruction.
   No parser assigns them meaning; a human or a quick-start agent does.

For the vocabulary these triggers reuse (sentinel, placeholder, entity URI,
selector), see [`glossary.md`](glossary.md); for task-file fields generally,
see [`simple-task-templates.md`](simple-task-templates.md).

---

## 1. Machine-parsed syntax

### `<<…>>` sentinels (task-file TOML)

A `<<…>>` value in a sentinel-eligible field is **resolved at squad time**
rather than taken literally. It is never treated as a literal branch name,
and any `<<…>>` value that is not a recognized sentinel is rejected at
validation time — a typo fails fast at submit, before the daemon sees it.

| Sentinel | Field | What it resolves to |
|---|---|---|
| `<<task:<name>[/<cell>]>>` | cell `upstream` | the tip of that dependency task's (or cell's) branch — the cell's branch rebases onto it |
| `<<ralphus:new-worktree/<branch>?upstream=<upstream>>>` | cell `cwd` | a materialized (or reused) git worktree for `<branch>`, created under the owning task's `project`; the stored `cwd` is rewritten to the real path before the cell runs (RAL-100) |
| `<<default>>` | the `?upstream=` value of a new-worktree placeholder | the repository's default branch — recommended |
| `<<current_branch>>` | the `?upstream=` value of a new-worktree placeholder | whatever branch the project currently has checked out — riskier, since it can change between runs |
| `<<review:<id>>>` | `[[task.cell]].review` | an existing `[[review]]`'s id in the same submission (RAL-269) |
| `<<ralphus:new-review/<key>>>` | `[[task.cell]].review` | mints a *fresh* review per submission; `<key>` groups the cells that share one new review (RAL-269) |
| `<<ralphus:linked-field/<path>>>` | `environment` value (task, cell, proof step, or `[[default]]`) | the field `<path>` addresses relative to where this value is declared — `cwd`/`id`, or a same-table sibling `environment.<key>` (RAL-460) |

Rules and risks:

- **Wrapping is required.** A bare (unwrapped) `ralphus:new-worktree/<branch>`
  placeholder cwd is a validation error — only the wrapped `<<…>>` form is
  accepted for newly authored TOML. The bare form still resolves for
  already-submitted data, but nothing teaches it, and it does not compose.
- **Sentinels are field-scoped, not global.** Only the fields above (and
  `[[]].review` generally) resolve sentinels. A `<<…>>` in a cell's `prompt`
  is just characters a model will read verbatim. Cell `environment` values
  are the one exception among free-text fields: when the owning task has a
  `project`, a wrapped new-worktree placeholder repeated there expands to
  the *same* materialized path as the `cwd` (memoized by the literal
  placeholder string, so the same value repeated across cells only
  materializes once). An `environment` value may instead be a **linked
  field** (RAL-460, below), which points back at another field on the same
  cell instead of repeating that field's placeholder text.
- **`?upstream=` is required** on a new-worktree placeholder cwd, so ralphus
  always knows what the created branch tracks instead of guessing from HEAD.
- **`depends_on` is deliberately not a sentinel** — it is a bare-string
  lookup into IDs that already exist in the file; nothing about it is
  resolved or modified later.
- **Don't conflate the two same-named `upstream`s** (RAL-284): a
  `[[review]] upstream` declares the review's **base branch**; a cell
  placeholder's `?upstream=` sets *that cell's worktree's* git tracking
  upstream at creation time. See the glossary's Reviews section.

### Linked field (`environment`, RAL-460)

An `environment` value may link to another field instead of re-embedding
that field's placeholder text (or its eventual resolved value). `.` in the
path addresses the table this `environment` entry is itself declared on;
`..` walks up one level of TOML nesting per repetition — a cell's proof
step's `..` reaches that cell, a cell's `..` reaches its owning task, a
task-scoped proof step's `..` reaches that task:

```toml
[[task.cell]]
id = "worker"
cwd = "<<ralphus:new-worktree/RAL-100-my-feature?upstream=main>>"
environment.WORKTREE = "<<ralphus:linked-field/./cwd>>"
environment.LOG_DIR = "<<ralphus:linked-field/./cwd>>/logs"

[[task.cell.proof]]
command = "cargo test"
environment.CELL_ID = "<<ralphus:linked-field/../id>>"
```

`environment.WORKTREE` resolves to this cell's own `cwd`, once the daemon has
materialized (or reused) that worktree — the real on-disk path, exactly the
same value `cwd` itself ends up holding, and at the same point in
materialization timing. `environment.LOG_DIR` resolves to that same path plus
the literal `/logs` text after the sentinel. The proof step's
`environment.CELL_ID` walks `..` up to its owning cell and reads that cell's
`id` ("worker").

Grammar:

```
"<<ralphus:linked-field/<path>>>"
```

`<path>` is `/`-separated: every segment before the last must be exactly `.`
or `..`; the last segment names the target field.

- **Embeds like a worktree placeholder.** A linked field is found by the same
  `<<...>>` scan a worktree placeholder is (`core::schema::text_placeholders`)
  — trailing literal text after the closing `>>` (as `LOG_DIR` above) is
  literal text appended to the resolved value, not a separate `?suffix=`
  query.
- **The target field** is `cwd`, `id`, or `environment.<key>` for a sibling
  entry in the SAME table's own `environment` (never an ancestor's — see the
  cross-scope restriction below). Which of `cwd`/`id` actually exist depends
  on what kind of table the path's navigation reaches: a cell has both, a
  proof step has only `id`, a task has neither (only `environment.<key>`).
  No other field names are recognized.
- **`environment.<key>` targets are same-table only (`.`, not `..`).** The
  daemon resolves one scope's whole `environment` table as a single
  already-hierarchy-merged map (`squad < task < cell < proof`, RAL-150)
  before any placeholder in it is resolved, so an ancestor's `<key>` is
  already present in that same merged map under its own name (or shadowed by
  a same-named override closer in) — there is no separate ancestor table left
  to address unambiguously by the time resolution runs. `cwd` and `id` don't
  have this problem (each level tracks its own literal value, never merged),
  so only they may cross a `..`.
- **Resolution timing.** A linked field resolves once its target field has a
  real value: linking to `cwd` resolves once that cell's own worktree
  placeholder (if it had one) has been materialized — the same point at
  which any other `cwd`-embedding `environment` value already resolves.
  Linking to a target that was never a placeholder (a cell `cwd` that's
  already a plain literal path, or a static `id`) resolves directly to that
  literal — there's no placeholder-resolution pass to run, just a value to
  read.
- **Chaining.** A linked field may itself be the target of another linked
  field, as long as `environment.<key>` targets stay same-table:

  ```toml
  [[task.cell]]
  cwd = "<<ralphus:new-worktree/RAL-100-my-feature?upstream=main>>"
  environment.BASE = "<<ralphus:linked-field/./cwd>>"
  environment.LOG_DIR = "<<ralphus:linked-field/./environment.BASE>>/logs"
  ```

  Here `LOG_DIR` links to `BASE`, which itself links to `cwd`. The daemon
  resolves these in dependency order — `cwd`, then `BASE`, then `LOG_DIR` —
  regardless of which order the `environment` keys happen to be declared or
  iterated in. A chain that cycles back on itself (`A` links to `B`, `B`
  links to `A`) is rejected: at submit time when the cycle is fully declared
  in one `environment` table, and defensively at run time either way.
- **Validation.** The linked-to field must actually exist at the level the
  path's navigation reaches, or submission fails validation — typo
  protection, no silent fallback. Walking `..` past the top of the
  submission, linking `cwd`/`id` to a level that doesn't have one (a task has
  neither; a proof step's own `id` must actually be set in the TOML, unlike a
  cell's, which falls back to an auto-generated one), or a same-table
  `environment.<key>` that doesn't exist there, are all hard validation
  failures. A malformed path (missing the leading `./`/`../`, an invalid
  navigation segment, no field name at the end) fails validation with a
  message naming the problem.
- **`[[default]].environment`** (RAL-460 follow-up) sets a linked field once
  and has it apply to every task/cell/proof step in the submission — it seeds
  the same squad layer of the hierarchical env-override store (RAL-150) a
  task's or cell's own `environment` seeds its own layer of, so `.`/`..`
  resolve against whichever task/cell/proof step it ends up applying to, not
  a single fixed table:

  ```toml
  [[default]]
  environment.WORKTREE = "<<ralphus:linked-field/./cwd>>"

  [[task]]
  name = "t"
  [[task.cell]]
  cwd = "<<ralphus:new-worktree/RAL-100-my-feature?upstream=main>>"
  ```

  Here every cell in the submission gets its own `WORKTREE` env var pointing
  at its own `cwd` — resolved per cell, not once. Because a default's
  `environment` entry is polymorphic like this, its linked-field syntax is
  only checked at submit time (a well-formed `./cwd`-shaped path); a target
  that doesn't apply at some particular cell/task/proof step surfaces as a
  runtime resolution error there instead, the same way an unresolvable
  placeholder anywhere else does. A task's own `environment` (inherited by
  every cell under that task) is validated the same leniently.

### `restart_on` grammar (proof steps)

A proof step's `restart_on` entries are parsed, not looked up: each is
`task/cell/proof?on=pass|fail|both` naming another proof step and which of
its verdicts should re-run this cell's proof cursor from the start.
Wildcards `task/*` and `task/cell/*` are recognized. Anything else in the
list is a validation error — there is no free-text form.

### Entity URIs and CLI selectors

The URI grammar (RAL-188) is parsed wherever a selector is accepted — CLI
commands, board address strings, and the daemon's `?id=`-carrying references:

```text
ralphus:/SQUAD[<label>]/TASK[<name>]/CELL[<name>]/PROOF[<name-or-~index>]   ?id=<squad_id>
ralphus:/REVIEW[<name>]                                                     ?id=<guardian_id>
ralphus:/REVIEW[<name>]?id=<guardian_id>&worktree=<branch-or-~index>&combined
```

- **`?id=` disambiguates.** Labels are neither unique nor stable; `?id=` is
  authoritative when present, and the CLI error messages tell you to add it
  when a review *name* matches several guardians.
- **`~` marks a positional index** (`PROOF[~0]`, `?worktree=~2`), so an index
  can never be confused with a proof *named* `0`.
- Square brackets delimit labels; `[`, `]` and `/` inside a label are
  percent-encoded, and balanced groups are extracted before splitting on `/`.

The CLI also accepts **legacy selectors** where a URI is overkill:

- Squad selectors are `/`-separated paths after the squad id
  (`<squad-id>/<task>[/<cell>[/<proof>]]`).
- Review selectors split head from branch at the first `#` or `~`
  (`<guardian-id>#<branch>`); a bare head is an id, and `@<name>` matches by
  review name. This is the shape you type for per-branch commands such as
  `review feedback <guardian-id>#<branch> "<text>"` — see the feedback
  section below for what that does.

### Unattended-reply markers (prompt cells and prompt proof steps)

Every `prompt` cell and `prompt` proof step gets ralphus's system
instructions appended to the agent's instructions, assembled in this order:
`## Background` (non-interactive framing) → `## Regarding Tools` (prefer
`rg`) → `## Conclusion` (async framing, then either the proof or the ghost
fragment). The markers that assembly teaches are parsed from the reply:

- **`RALPHUS_PROOF: PASS` / `RALPHUS_PROOF: FAIL`** — the verdict contract of
  a `prompt` proof step. Applies to prompt proof steps only: normal cells
  never get the proof fragment. Parsing takes whichever of the two markers
  occurs **last** in the reply, since re-invocations can carry earlier,
  superseded verdicts — and it does not require the marker alone on its own
  line (local models sometimes embed it mid-sentence), even though the
  assembled prompt teaches a final exact line. A reply with no verdict at
  all is fail-closed (`proofed = false`).
- **`RALPHUS_STILL_WORKING: <one-line reason>`** — the continuation escape
  hatch. Applies to prompt cells **and** prompt proof steps (the async
  fragment is in both). When present, the runner re-invokes the same agent
  session synchronously with a follow-up prompt carrying the reason, a
  bounded number of attempts (`MAX_ASYNC_ATTEMPTS` — 3 today); exhausted
  attempts hard-fail a normal cell and fail-close a proof step. Only the
  marker and the reason text on its line are parsed — the surrounding
  framing in the prompt is instruction, not contract.
- **`RALPHUS_GHOST: <up to 5 short bullet points>`** (or
  `RALPHUS_GHOST: (nothing to report)`) — the handoff note of a prompt cell
  (RAL-136). Applies to prompt cells; proof steps get the verdict contract
  instead of the ghost fragment. Everything after the last marker — to the
  end of the reply, case-insensitively ignoring the `(nothing to report)`
  closer — is extracted into the cell's **ghost** record (capped at a size
  limit), which is injected as "Prior context" into the cell's own restarts
  and one level of dependent cells' prompts — that is its whole routing
  scope. The bullet-count and "not a changelog" framing are prompt
  conventions, not parser-enforced shapes; only the marker line is parsed.

Risk worth knowing before you echo these markers: an agent whose *work*
happens to print one of these strings (e.g. an agent developing ralphus,
reading this guide) can trip the parser that reads its own output. The
known hardening is ralphus's own: parse what the prompt taught narrowly, and
prefer a standalone exact-form line over a substring scan.

### Process-side sentinels (agent authors, not cell authors)

Three sentinels exist for code that *hosts* a ralphus runner, not for task
files or cell replies:

- **`RALPHUS_EVENT: {"level":"info",…}`** — stderr marker lines the runner
  subprocess (and a machine provider reaching a remote host) emits so live
  usage events reach the daemon's Cartographer event log. If you build a
  machine provider, print these on stderr; the daemon's stderr-reading
  thread forwards them. Note the provider must not block its own stderr pipe
  on the forwarding thread.
- **`RALPHUS_TMUX_DONE: <status>`** — a stdout sentinel the runner prints
  into its tmux pane once it has written its result file (RAL-102). The
  daemon polls pane content for a **standalone line of that exact form** —
  deliberately not a substring scan, which false-positives whenever the
  cell's own work echoes the literal marker.
- **`RALPHUS_THINKING: <one line of reasoning>`** — a stdout marker the
  runner prints before each line of model thinking/reasoning content, for
  backends that report thinking as its own distinct event stream (RAL-434;
  pi today, via its `--mode json` `thinking_start`/`thinking_delta`/
  `thinking_end` events). Unlike the two above, this tags the *model's*
  output rather than ralphus's own telemetry, and it exists so thinking
  visibility is a **render-time** choice: the board's per-pane "Show
  Thinking" checkbox strips the tag when on and folds the run of lines
  carrying it into one `⟨thinking…⟩` placeholder when off. Nothing is
  discarded at capture time, so folding is freely reversible; `[live_view]
  hide_thinking` only sets which way a newly-opened pane starts.

  Matched as a **line prefix** (after any leading whitespace), never as a
  substring, so agent output that merely echoes the literal marker
  mid-line is left alone. The `/pane` fallback view has no checkbox of its
  own, so the daemon strips the tag there and shows the reasoning plainly
  (`server.rs::unprefix_thinking_line`).

---

## 2. Agent-facing conventions (not parsed)

These influence behavior because a person — or a quick-start agent told to
use them — maps them onto a CLI command. No parser reads them, and this
distinction is the single most common misunderstanding in this area.

### `feedback` in reviewer mode

The conversational word `feedback` is **not** hard-coded parser syntax. Its
importance comes from two ralphus-specific places:

- the CLI's `review feedback` command — `ralphus review feedback
  <selector> "<text>"` — and
- the reviewer quick-start system prompt (`REVIEWER_ROLE_NOTE` in
  `cli/src/commands/quick_start.rs`), which tells any agent operating an
  existing review that **any requested code change** ("fix X", "rename Y",
  "add a test for Z") must be sent as that command, never made by editing a
  review worktree directly.

What sending feedback routes:

- **Scope: one branch of one review.** The selector targets a single
  guardian branch (`<guardian-id>#<branch>`); the text is posted to that
  branch's read-only feedback thread and triggers that branch's own
  automated resolver — the conflict-resolving agent — to re-attempt the
  change, amend, and push under the review's gating and audit trail.
- **One or more review trees, via the PR bridge.** Forge (PR/MR comment)
  feedback reaches a review's worktrees through the manual "Pull in PR
  feedback" action — CLI `review pr pull-feedback <pr-id>` — which fetches
  the PR's un-actioned comments, concatenates them into one message, and
  routes it to **one** branch: the branch the PR was filed for, or, for a
  whole-stack PR (filed from the combined worktree), the topmost enabled
  stacked branch of the owning review. Different PRs route to different
  branches; a single pull always lands in exactly one branch's worktree. (The review's `auto_pr_feedback` setting persists the
  intent to do this automatically, but it is data-model only — no poller
  consumes it yet — so today every PR-feedback pull is deliberate.)
- **Ambiguity risks:**
  - Feedback meant for one branch routes wherever its selector points —
    check the `#<branch>` half before sending.
  - For a whole-stack PR (filed from the combined worktree), the fallback
    target — the topmost enabled stacked branch — is a guess; file the PR
    from the branch you actually want comment feedback to reach.
  - "One target" means one *edited* branch, not one effect: after the
    resolver edits branch A, every branch downstream of A in the stack is
    re-stacked on top and the combined worktree is rebuilt. Only the task
    worktrees are never touched.
- **The hard boundary behind the convention:** editing a review worktree
  file directly bypasses the resolver, gating, and audit trail. It is never
  correct in reviewer mode, no matter how small the change looks. The
  quick-start prompt states this in its CRITICAL paragraph precisely because
  prior sessions reached for the visible, writable worktree instead of the
  command.
- **The resolver's own git-command boundary.** The automated resolver
  `review feedback` dispatches into the branch's worktree may run read-only
  git commands (`git status`, `git diff`, `git log`, etc.) to inspect the
  worktree or the diff so far — that's allowed and often necessary to make an
  accurate, scoped edit. It must not commit or push itself; a separate,
  dedicated commit-step call stages and commits the genuine part of the
  diff afterward, preserving the same gating and audit trail.
- **Why the word itself feels loaded:** in the board's per-branch feedback
  threads, a human types a note and the guardian agent applies it — the word
  appears in every review conversation, but nothing parses it. It routes
  only because someone (human or quick-start agent) types
  `review feedback` or follows the quick-start instruction that maps change
  requests to it.

### Ghost framing conventions

The unattended prompt teaches ghost hygiene: "up to 5 short bullet points",
"things a future agent could NOT learn from `git log` or the diff", "this is
not a changelog", and the exact closer `RALPHUS_GHOST: (nothing to report)`.
Only the marker line is parsed; the rest is style the prompt asks for.
Agents following the convention produce better handoff notes, but a ghost
that ignores the bullet rules is still accepted, extracted, and routed
exactly the same.

### Quick-start mode framing

`ralphus quick-start` generates full system prompts — manager mode
("You are Ralphus. You orchestrate the `ralphus` CLI…") and reviewer mode
("You are Ralphus operating in REVIEWER mode… CRITICAL — you never edit,
commit, or push code yourself…"). The specific phrases carry no weight to
any parser; they are templates you can reuse when writing your own agent
harness. The conventions they teach (prefer `rg`, never ask a clarifying
question, send change requests as feedback) only shape model behavior
because the model reads them as instructions.

---

## Safe usage — saying ordinary things without triggering anything

- **`<<…>>` is field-scoped.** In a cell's `prompt` or a review's feedback
  text it is literal characters. Only the TOML fields listed in the
  sentinel table resolve anything — and an unrecognized sentinel there
  fails validation rather than silently resolving.
- **The reply markers are only read from an agent's *reply*, and only for
  prompt cells and prompt proof steps.** Quoting them in documentation or
  conversation is inert. If you are *writing* a task whose subject is
  ralphus itself, echoing marker text in a model's final line is the one
  reachable trap: the last-occurrence-wins verdict parse exists to narrow,
  not eliminate, that — ralphus's own completion sentinel handles the known
  case by requiring a standalone line of exact form rather than a substring
  scan.
- **`feedback`, `review`, `restart`, `resolve` and other conversational
  words are never parsed.** They trigger nothing until they become a CLI
  invocation or an agent instruction. Say them freely in prose.
- **Selectors, unlike sentinels, are accepted verbatim.** A `#<branch>` in
  a selector is machine-parsed wherever that command takes a selector — so
  check the head and branch halves before running per-branch commands.

## See also

- [`glossary.md`](glossary.md) — the taken words these triggers reuse
  (**sentinel**, **placeholder**, **ralphus URI**, **selector**, **ghost**)
- [`simple-task-templates.md`](simple-task-templates.md) — the task-file
  fields the sentinels appear in
- [`daemon-api.md`](daemon-api.md) — the wire shapes selectors and URIs
  address
- `ralphus task show-tutor` — the CLI's own teaching of the wrapped-sentinel
  forms
