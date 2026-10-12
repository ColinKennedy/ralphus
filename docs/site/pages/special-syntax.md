# Special syntax & markers

Words that look ordinary but trigger special ralphus behavior. Two
categories, kept separate:

- **Machine-parsed syntax** — validated, resolved, or parsed by ralphus
  code. Same behavior no matter who types it.
- **Agent-facing conventions** — phrases that route only because a person or
  a quick-start agent maps them onto a CLI command. No parser reads them.

The full guide — every rule, risk, and ambiguity, including safe usage —
lives in the repository at `docs/special-syntax.md` (start at the repo root
on GitHub, or open the file locally). This page is the quick table.

## Machine-parsed triggers

| Trigger | Applies to | What it does |
|---|---|---|
| `<<task:<name>>` | cell `upstream` | rebases the cell's branch onto that dependency task's branch tip at squad time |
| `<<ralphus:new-worktree/<branch>?upstream_remote=origin&upstream_branch=<upstream>>>` | cell `cwd` | materializes (or reuses) a worktree for `<branch>` under the task's `project`; the stored `cwd` is rewritten to the real path |
| `<<default>>` / `<<current_branch>>` | the placeholder's `?upstream_branch=` value | the branch the new worktree tracks: the repo's default branch (recommended), or whatever the project has checked out (changes between runs) |
| `<<review:<id>>>` / `<<ralphus:new-review/<key>>>` | `[[task.cell]].review` | attaches the cell to an existing review, or mints a fresh review per submission (`<key>` groups cells into one) |
| `<<ralphus:review/<id>>>` / `<<ralphus:squad/<id>>>` / `<<ralphus:new-review/<key>>>` / `<<ralphus:new-squad>>` | `[[waypoint]].roster` and `.affected` | names an existing review or squad, a same-file review, or the squad this submission itself creates (no `<key>` — one file makes exactly one squad). `roster` is the completion list that releases the hold; a bare id is a validation error. The older `<<review:<id>>>` / `<<squad:<id>>>` forms are still accepted |
| `task/cell/proof?on=pass\|fail\|both` | proof step `restart_on` | when the named proof step fires with that verdict, this cell's proof cursor re-runs from the start |
| `ralphus:/SQUAD[…]…` | CLI selectors, board, API | addresses any squad/task/cell/proof/review entity; `?id=` disambiguates, `~` marks a positional index |
| `<guardian-id>#<branch>` | per-branch review commands | legacy review selector: head is the review (id or `@name`), after `#`/`~` is the branch |
| `RALPHUS_PROOF: PASS` / `FAIL` | prompt proof steps | the verdict; the **last** occurrence in the reply wins; none at all is fail-closed |
| `RALPHUS_STILL_WORKING: <reason>` | prompt cells and prompt proof steps | the runner re-invokes the same session synchronously with the reason, bounded attempts |
| `RALPHUS_GHOST: <bullets>` | prompt cells | the handoff note; extracted into the cell's ghost record and injected as prior context into restarts and one level of dependent cells |
| `RALPHUS_EVENT: {…}` | runner/machine-provider stderr | forwarded into the daemon's Cartographer event log |

`<<…>>` values are **reserved** — never treated as literal branch names, and
any unrecognized one is rejected at validation time. `depends_on` is
deliberately not a sentinel. Unknown `restart_on` entries are validation
errors.

Every `prompt` cell/proof step also gets an unconditional `## Cross-Squad
Waypoints` system-prompt section (RAL-400), and a rostered cell additionally
gets a **bearing block** prepended to its own prompt — coordination context
an agent must reconcile against what it can actually observe, never a
substitute for inspecting the working tree itself. The exact injected text
and block format are in the repository's `docs/special-syntax.md`, under
"Cross-squad waypoint bearing injection (RAL-400)".

### `initialization_commands` (worktree init, RAL-615)

`initialization_commands` (a string or a list of strings) and
`initialization_timeout_sec` (> 0) on a cell, a task, `[[default]]`, or the
project's `.ralphus.toml` `[worktree]` table name command(s) that run **once**
in each freshly created cell worktree, after `git worktree add` and upstream
setup and before any cell uses it. A list runs in order and stops at the first
failure; the timeout covers the whole list (default 900s). An empty list or a
newline inside a command is invalid. `""` disables an inherited value and is an
error when nothing is inherited.

Precedence: cell > task > squad `[[default]]` > `.ralphus.toml` `[worktree]`,
resolved once at submit time and stored (with its source) on each cell. Cells
sharing one worktree placeholder must resolve to the same command.

A non-zero exit or a timeout fails every cell bound to the worktree, no agent
starts, and the worktree stays on disk. Fix the command with the cell edit
form (`ralphus cell edit --initialization-commands`), then Retry: Retry re-runs
the init in place when the last run failed or the command was edited, and
recreates the worktree from its original placeholder first if the directory is
gone. A resync of an existing, successfully initialized worktree does not
re-run it. Trust level: the commands run through the platform shell with the daemon's privileges, the same level as `[review] auto_build`; they come from the task file or the git-tracked `.ralphus.toml`. Write them to be idempotent: a Retry after a failure re-runs the whole list in place.

## Agent-facing conventions (not parsed)

| Phrase | Where | What actually routes |
|---|---|---|
| "feedback" / a change request ("fix X") | reviewer mode (a human, or a `quick-start reviewer` agent) | `ralphus review feedback <guardian-id>#<branch> "<text>"` — posts to **one** branch's feedback thread and triggers that branch's resolver; PR comments reach a branch only via the deliberate `review pr pull-feedback <pr-id>` action |
| "You are Ralphus…" / the quick-start system prompts | generated by `ralphus quick-start manager/reviewer` | templates for your own agent harness; the phrases carry no parser weight |

The hard boundary behind reviewer mode: **never edit a review worktree
directly** — that bypasses the resolver, gating, and audit trail. Send
change requests as feedback instead.
