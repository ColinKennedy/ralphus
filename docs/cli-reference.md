# ralphus CLI reference

`ralphus` is a thin, stateless HTTP client over the daemon API
(`docs/daemon-api.md`) — every subcommand below maps to one or more daemon
endpoints. See `CLI_PARITY_PLAN.local.md` for the design history.

## Global flags

- `--daemon-url URL` — base URL of the daemon (default `http://127.0.0.1:7890`,
  or `$RALPHUS_DAEMON_URL`).
- `$RALPHUS_DAEMON_TIMEOUT` — per-request HTTP timeout in seconds (default `60`).
  The daemon's HTTP loop is single-threaded, and a mutating request like `submit`
  can involve TOML validation, project-registry lookups, and git calls during
  review derivation, so a busy daemon can take longer than a health check to
  respond. No CLI flag yet — env var only.
- `--json` — emit the raw daemon JSON instead of human-readable text. Works on
  every subcommand that reads or mutates daemon state.
- `--version` — print the CLI version and exit.

## Exit codes

| Code | Meaning |
|---|---|
| 0 | ok |
| 1 | domain error (daemon reached, request rejected) |
| 2 | usage/local error (bad args, file unreadable, unresolved selector) |
| 3 | not found (HTTP 404) |
| 4 | conflict (HTTP 409, e.g. "already merging") |

## Selectors

Most `show`/action commands take a **selector** instead of raw ids/indices
(`cli/src/ralphus/selector.py`):

| Selector | Addresses |
|---|---|
| `<run_id>` | a run |
| `<run_id>/<task>` | a task node (`task` = index or name) |
| `<run_id>/<task>/<session>` | a session (`session` = index or name) |
| `<run_id>/<task>/verify/<i>` | a task-level verify step |
| `<run_id>/<task>/<session>/verify/<i>` | a session-level verify step |
| `<guardian_id>` or `@<name>` | a review |
| `<guardian_id>#<pos-or-branch>` or `@<name>#<pos-or-branch>` | a review branch |

A name segment that matches more than one candidate, or none, exits 2 with the
list of candidates.

## Core

| Command | What |
|---|---|
| `validate <file>` | Validate a task TOML file offline (no daemon needed if `ralphus-daemon` is on PATH) |
| `submit <file...\|dir\|glob\|-> [--label] [--hold] [--activate] [--wait] [--no-validate]` | Submit task TOML. See [Submit](#submit) below |
| `author [--goal\|--prompt-file] [--verify] [--review] [--hold] [--dry-run] ...` | Generate, validate, and submit a TOML from a plain-language goal |
| `status [run_id] [--concurrency]` | Show one run, or list all runs; `--concurrency` shows scheduler load instead |
| `resources` | Per-task CPU/RAM/GPU for running sessions |
| `graph [run_id] [--global] [--all] [--format ascii\|dot]` | Render the task-order dependency graph |
| `get <selector> [field.path]` | jq-lite field query over any entity's JSON view |
| `clear [--all\|--status STATES] [--keep-temporary] [--yes]` | Bulk-delete tasks and reviews |
| `check health [--enable-developer-checks]` | System/environment health check |
| `completion bash` | Print a bash tab-completion script |
| `configuration show [--no-local]` | Show sourced `.ralphus.toml` files and resolved values |
| `task show-tutor` | Print the Task TOML schema reference |
| `queue list [--all]` / `reorder <paths...>` / `set-position <paths...> --to N [--relative]` / `set-status <path> <state>` | Inspect/reorder the run queue by priority |
| `initialize git [--path]` | Enable git rerere in a repository |

### Submit

`submit` accepts one or more sources, each of which may be a `.toml` file
path, a directory (expands to every `*.toml` file in it, non-recursive), a
glob pattern (shell-quote it so `ralphus` expands it, not your shell), or `-`
for stdin. Explicit file paths combine into **one** run (so a shared
`ralphus:new-review/<key>` folds into a single review); a directory or glob
submits **each matching file as its own separate run** instead (one failure
aborts the remaining batch).

- `--hold` stages as `queued` instead of `pending`.
- `--activate` forces `--hold` internally, submits, then immediately calls
  `run activate` — "submit held then promote" in one step.
- `--wait` polls each submitted run and prints its state until terminal;
  exits 0 only if the run reached `done` (1 for `failed`/`cancelled`).
- `submit` **validates before submitting by default** (a client-side
  `POST /api/runs/validate` call) and prints the full per-line
  `error [line N]: message` report instead of submitting if it fails. In a
  batch (directory/glob), each file is validated right before its own
  submit, so one invalid file blocks the rest of the batch. `--no-validate`
  skips this pass and submits directly — the daemon still rejects invalid
  TOML server-side either way, so this doesn't let anything through; it only
  trades the fuller pre-submit report for the daemon's single summary error
  message. To validate without submitting at all, use `ralphus validate`.

## run

| Command | Endpoint |
|---|---|
| `run list [--status] [--name] [--sort]` | `GET /api/tasks` (server-side filtered/sorted) |
| `run show <run_id>` | `GET /api/runs/{id}` |
| `run logs <run_id>` | `GET /api/runs/{id}/logs` |
| `run set-status <run_id> <state>` | `POST /api/runs/{id}/set-status` |
| `run restart <run_id>` | `POST /api/runs/{id}/restart` |
| `run retry <run_id>` | `POST /api/runs/{id}/retry` |
| `run activate <run_id>` | `POST /api/runs/{id}/activate` |
| `run cancel <run_id>` | `POST /api/runs/{id}/cancel` |
| `run delete <run_id> [--yes]` | `DELETE /api/runs/{id}` |
| `run rename <run_id> <label>` | `POST /api/runs/{id}/edit` |
| `run edit <run_id> [--label]` | `POST /api/runs/{id}/edit` |

## task

| Command | What |
|---|---|
| `task show <selector>` | Task node detail |
| `task set-status <selector> <state>` | Override a task's status |
| `task restart-verify <selector> --from <i>` | Restart task-level verify steps from index `i` |
| `task edit <selector> [--name] [--project]` | Edit a task node's name/project |

## session

| Command | What |
|---|---|
| `session show <selector>` | Session detail (incl. tokens, cost, agent, model, cwd) |
| `session worktree <selector>` | The worktree/project a session is using |
| `session reviews <selector>` | Reviews this session's branch participates in |
| `session set-status <selector> <state>` | Override a session's status |
| `session restart <selector>` | Restart a session and its downstream |
| `session restart-verify <selector> --from <i>` | Restart session-level verify steps from index `i` |
| `session edit <selector> [--cwd] [--agent] [--model] [--prompt] [--command]` | Edit a session's fields |
| `session terminal <selector> [--mode open\|readonly]` | Print the `claude --resume` command + cwd (does **not** ask the daemon to spawn a terminal — see below) |

## verify

| Command | What |
|---|---|
| `verify show <selector>` | Verify step detail (kind, state, spec, output) |
| `verify set-status <selector> <state>` | Override a verify step's status |
| `verify restart <selector>` | Restart a verify step (dispatches to the task- or session-scoped restart endpoint) |

## review

| Command | What |
|---|---|
| `review list [--status]` | List reviews |
| `review show <selector>` | Review detail, incl. `ready`/`merge_progress`/`summary_state` |
| `review logs <selector>` | State-transition audit log |
| `review status <selector>` | Per-branch `merge_status`/`ready`/detail + a summary verdict — "is this review ready?" |
| `review worktrees <selector>` | Worktrees/branches this review consumes, with source session |
| `review create <name> <base_branch> <git_root> [--checks] [--skip-checks] [--skip-worktrees] [--review-type]` | Create a review |
| `review rename <selector> <name>` | Rename |
| `review cancel <selector>` | Cancel |
| `review delete <selector> [--yes]` | Delete + purge worktrees |
| `review settings <selector> [--skip-checks] [--skip-worktrees] [--resolver-agent] [--resolver-model] [--base-branch]` | Update opt-out settings |
| `review add-branch <selector> <branch>` | Add a branch |
| `review reorder <selector> <b1,b2,...> [--disable b,b] [--enable b,b]` | Reorder branches + rebase, atomically |
| `review branch enable\|disable <selector#branch>` | Enable/disable one branch + rebase |
| `review merge <selector>` | Start/continue the stacked rebase |
| `review restart-merge <selector>` | Cancel an in-progress rebase, start fresh |
| `review force-start <selector>` | Disable not-yet-done branches, merge immediately |
| `review approve <selector>` | Approve an in_review review |
| `review feedback <selector#branch> <text>` | Post feedback on a branch |
| `review dismiss-reenable <selector#branch>` | Dismiss the "can re-enable" notice |
| `review move-branch <selector#branch> <to_review>` | Move a branch to another review + rebuild both (RAL-118) |
| `review base list <selector>` | Candidate base branches |
| `review base set <selector> <branch>` | Change base branch + rebuild |
| `review checks list <selector>` | List LLM-synthesized manual review-verification commands |
| `review checks run <selector> [--index N...] [--all] [--input NAME=VALUE...]` | Print command(s) + cwd for one/some/all checks (not run by the daemon — see below) |
| `review action list <selector>` | List user-declared `[[review.action]]` hints |
| `review action run <selector> --index N [--input NAME=VALUE...]` | Print the command + cwd for a `command`-kind hint |
| `review chat send <selector> <text>` | Post to the global feedback thread |
| `review chat show <selector>` | Show the feedback thread |
| `review chat fork <selector> --seq N <text>` | Fork the thread at a message |
| `review pr submit <selector> (--position N \| --combined) [--alias] [--title] [--description]` | Submit a PR/MR (RAL-117); title/description default to an LLM suggestion |
| `review pr list <selector>` | List PRs submitted for a review |
| `review pr show <pr_id>` | Show one PR row |
| `review pr find <forge> <repo> <pr_number>` | Look up the PR row for a forge PR/MR number |
| `review pr update <pr_id> [--pr-number] [--pr-url] [--branch-alias] [--state]` | Mutate the recorded PR mapping (e.g. after a reopen) |
| `review pr comments <pr_id>` | List a PR's comments, flagging which are already actioned |
| `review pr pull-feedback <pr_id>` | Action a PR's un-actioned feedback into the owning worktree |

### Why some commands print instead of act

`session terminal`, `review checks run`, and `review action run` deliberately
print a resolved shell command + working directory instead of calling the
daemon's `open-terminal` / `run-manual-commands` / `run-action-hint`
endpoints. Those endpoints spawn a GUI terminal window **on whatever machine
runs `ralphus-daemon`** — meaningful for the browser board (same desktop
session) but not for a headless/remote CLI invocation. Run the printed
command yourself instead.

### Structured check inputs (RAL-164)

A check (manual or `[[review.action]]`) may declare named `inputs` —
placeholders like `{port}` in its command — instead of hardcoding a value
that could collide across concurrent reviews. `review checks run`/`review
action run` substitute them before printing: a repeatable `--input
NAME=VALUE` flag wins, falling back to the review's last-used value for that
name, falling back to the input's own declared default. If an input ends up
with no value from any source, the command errors listing the missing
input name(s) instead of printing a command with a bare, unsubstituted
`{name}` left in it.

## show

| Command | What |
|---|---|
| `show help-map` | Print the full CLI command surface as an alphabetized, indented tree (see [Machine-readable help-map](#machine-readable-help-map-ral-110) below) |

## quick-start

| Command | What |
|---|---|
| `quick-start claude-code [--command] [-- ARGS...]` | Launch Claude Code primed with the full CLI help-map as a system prompt, so it can orchestrate `ralphus` unsupervised. See [quick-start claude-code](#quick-start-claude-code) below |

### quick-start claude-code

Writes the help-map (same tree `show help-map` prints) to a throwaway temp
file and launches `claude --dangerously-skip-permissions
--append-system-prompt-file <tempfile> ...`, so the file's contents become
Claude Code's system prompt via the dedicated file-based flag (the path is
its own argument value, not embedded in a text value, so it can't be
truncated by a space in the path).

- Args after a literal `--` are forwarded verbatim to the underlying `claude`
  invocation, e.g. `ralphus quick-start claude-code -- --mode auto`.
- If the forwarded args include their own `--append-system-prompt-file`, its
  contents are read and folded into ralphus's own temp file instead —
  ralphus's context first, then a disclaimer, then the user's — rather than
  forwarding a second, separate occurrence of the flag.
- The `claude` launch command resolves, in order: `--command` (this
  invocation only), then `$RALPHUS_CLAUDE_COMMAND`, then the bare `claude` on
  PATH. The value may be a single bare path, or a compound shell command with
  embedded spaces/syntax (e.g. `cd foo bar ; ./claude`) — see the heuristic in
  [`check health`](#core) above. **A bare path containing a space (e.g. a
  Windows install under `C:\Program Files\...`) must be wrapped in quotes**
  (`"C:\Program Files\claude\claude.exe"`) or it is misdetected as a compound
  command (RAL-110 Q5: any space without fully-wrapping quotes means
  "compound").
- `$RALPHUS_CLAUDE_COMMAND` is the same env var the `claude-code` agent
  backend uses (`ralphus.runner.claude_code_backend`) — one name, everywhere
  a `claude` executable is resolved.

## Machine-readable help-map (RAL-110)

The tree below is generated by walking every subcommand with `--verbose
--help` (`ralphus.helpmap`, `uv run ralphus-help-map`) and is regenerated by
`uv run ralphus-docs-helpmap` (`--check` for CI drift detection — see
`.github/workflows/ci.yml`). Each node lists its positional args, then its
optional flags (each with a `[type]` hint — `--verbose --help` on any
subcommand shows the same hints live), then a `{one-line description}`, then
its subcommands.

`author` and `quick-start` are deliberately excluded from the tree
(`ralphus.helpmap._HIDDEN_COMMANDS`): `author` starts its own agentic
authoring loop, and an AI agent already driving `ralphus` via this help-map
should write and `submit` TOML directly rather than invoking a second
authoring agent through it; `quick-start claude-code` launches an entire
separate `claude` process, which the driving agent should never re-launch on
itself.

A hand-picked subset of commands (`ralphus.helpmap._SUBAGENT_PATHS`) carries
an inline `(subagent)` tag: `review`, `submit`, `clear`, and `check health`
today — each is slow/blocking, destructive, or does multi-step
subprocess/filesystem work whose result isn't needed synchronously. Cheap,
frequently-polled reads (`status`, `get`, ...) and tight edit-loop commands
(`validate`) are deliberately left untagged. Wherever this tree is shown to
an AI agent — `show help-map`, `quick-start claude-code`'s injected system
prompt, `ralphus-help-map`'s own stdout — it's preceded by
`ralphus.helpmap.SUBAGENT_NOTE`, which explains what the tag means: invoke
that command from inside a subagent (e.g. Claude Code's Task tool) rather
than directly in the driving agent's main context.

<!-- BEGIN GENERATED HELP-MAP (RAL-110) -->
```
- ralphus --daemon-url [str] --json --version  {Submit and manage autonomous agent tasks against the ralphus daemon.}
    - agent  {Inspect agent backends ralphus can run.}
        - list  {List supported agent backends and the models each is allowed to run.}
    - check  {System and environment checks.}
        - health --enable-developer-checks (subagent)  {Check the local ralphus setup (daemon, git, runner, ollama).}
    - clear --all --keep-temporary --status [str] --yes (subagent)  {Delete tasks and reviews from the daemon.}
    - completion shell [bash]  {Print a shell tab-completion script.}
    - configuration  {Configuration inspection.}
        - show --no-local  {Show sourced .ralphus.toml files and resolved values.}
    - get selector [str] field [str, optional]  {Query one field from any entity's JSON view (jq-lite).}
    - graph run_id [str, optional] --all --format [ascii|dot] --global  {Render the task-order dependency graph.}
    - history selector [str] --live --wait-until-valid [float, optional]  {Show a session/verify step's tmux history, or tail it live (RAL-140).}
    - initialize  {One-time local setup helpers for a repository.}
        - git --path [path]  {Enable git rerere in a repo so review rebases replay conflict resolutions.}
    - listen selector [str] --timeout [float] --until [str]  {Block until a run/task/session/verify/review/review-worktree reaches a status (RAL-140).}
    - project  {Register and inspect projects known to the daemon (RAL-100).}
        - get name [str]  {Show one registered project's details by exact name.}
        - git --description [str] --name [str] --path [path]  {Register a git repository as a project the daemon can resolve placeholder session cwds against.}
        - list --short  {List every project registered with the daemon.}
    - queue  {Inspect and reorder the run queue by priority.}
        - list --all  {List queued work items (ready-to-run by default).}
        - reorder paths [str, one or more]  {Set the queue order to the given item paths (dependency-repaired).}
        - set-position paths [str, one or more] --relative --to [int]  {Move item(s) to an absolute index or a relative offset.}
        - set-status path [str] state [str]  {Set a run/task/session/verify status (e.g. ignored) by item path or run id.}
    - resources  {Show per-task resource usage (CPU/RAM/GPU).}
    - retry selector [str] --env-file [path] --environment [str, repeatable] --unset-environment [str, repeatable]  {Re-run a run/task/session/verify step, optionally overriding environment variables (RAL-150).}
    - review (subagent)  {Inspect and act on reviews (guardians).}
        - action  {User-declared [[review.action]] test/action hints.}
            - list selector [str]  {List the action hints.}
            - run selector [str] --index [int] --input [str, repeatable]  {Print the command + cwd for a command-kind action hint.}
        - add-branch selector [str] branch [str]  {Add a branch to a review.}
        - approve selector [str]  {Approve a review that is in_review.}
        - base  {Inspect/change a review's base branch.}
            - list selector [str]  {List candidate base branches.}
            - set selector [str] branch [str]  {Change the base branch.}
        - branch  {Enable/disable one review branch.}
            - disable selector [str]  {Disable a branch and kick off the rebase.}
            - enable selector [str]  {Enable a branch and kick off the rebase.}
        - cancel selector [str]  {Cancel a review.}
        - chat  {The review's global feedback thread.}
            - fork selector [str] text [str] --seq [int]  {Fork the thread at a message, replacing it with new text.}
            - send selector [str] text [str]  {Post a message.}
            - show selector [str]  {Show the thread.}
        - checks  {LLM-synthesized manual review-verification commands.}
            - list selector [str]  {List the manual checks.}
            - run selector [str] --all --index [int, repeatable] --input [str, repeatable]  {Print the command(s) + cwd to run one/some/all manual checks yourself.}
        - create name [str] base_branch [str] git_root [str] --checks [str] --review-type [str] --skip-auto-build --skip-worktree-checks --skip-worktrees  {Create a new review.}
        - delete selector [str] --yes  {Delete a review and its worktrees.}
        - dismiss-reenable selector [str]  {Dismiss the 're-enable' notification for a branch.}
        - feedback selector [str] text [str]  {Post feedback on one branch, triggering a resolver re-attempt.}
        - force-start selector [str]  {Disable not-yet-done branches and merge immediately (only while collecting).}
        - list --status [str]  {List reviews.}
        - logs selector [str]  {Show a review's state-transition audit log.}
        - merge selector [str]  {Start (or continue) the stacked rebase.}
        - move-branch selector [str] to_review [str]  {Move a branch to another review (RAL-118), then rebuild both.}
        - pr  {Submit/query pull requests for a review (RAL-117).}
            - comments pr_id [str]  {List a PR's comments/notes.}
            - find forge [github|gitlab] repo [str] pr_number [int]  {Look up the ralphus PR row for a forge PR/MR number.}
            - list selector [str]  {List PRs submitted for a review.}
            - pull-feedback pr_id [str]  {Action a PR's un-actioned feedback into the owning review worktree.}
            - show pr_id [str]  {Show one PR row.}
            - submit selector [str] --alias [str] --combined --description [str] --position [int] --title [str]  {Submit a PR/MR for one stacked branch or the combined worktree.}
            - update pr_id [str] --branch-alias [str] --pr-number [int] --pr-url [str] --state [open|merged|closed]  {Mutate the recorded PR mapping, e.g. after a PR is closed and reopened under a new number.}
        - rename selector [str] name [str]  {Rename a review.}
        - reorder selector [str] order [str] --disable [str] --enable [str]  {Set the branch order and kick off the rebase.}
        - restart-merge selector [str]  {Cancel an in-progress rebase and start a fresh one.}
        - settings selector [str] --auto-pr-feedback, --no-auto-pr-feedback --base-branch [str] --resolver-agent [str] --resolver-model [str] --skip-auto-build, --no-skip-auto-build --skip-worktree-checks, --no-skip-worktree-checks --skip-worktrees, --no-skip-worktrees  {Update per-review opt-out settings.}
        - show selector [str]  {Show a single review's detail.}
        - status selector [str]  {Per-branch readiness + a summary verdict ('is this review ready?').}
        - worktrees selector [str]  {The worktrees/branches this review consumes.}
    - run  {Inspect and act on runs.}
        - activate run_id [str]  {Promote a held (queued) run to pending.}
        - cancel run_id [str]  {Cancel a run.}
        - delete run_id [str] --yes  {Permanently delete a run.}
        - edit run_id [str] --label [str]  {Edit a run's fields.}
        - list --name [str] --sort [date|name] --status [str]  {List runs.}
        - logs run_id [str]  {Show a run's state-transition audit log.}
        - rename run_id [str] label [str]  {Rename a run's label.}
        - restart run_id [str]  {Restart a whole run, dirtying every run that depends on it.}
        - retry run_id [str]  {Re-run with the same parameters (reset to pending).}
        - set-status run_id [str] state [str]  {Manually override a run's status.}
        - show run_id [str]  {Show a single run's detail.}
    - session  {Inspect and act on sessions.}
        - edit selector [str] --agent [str] --command [str] --cwd [str] --model [str] --prompt [str]  {Edit a session's fields.}
        - restart selector [str]  {Restart a session (and its downstream), dirtying dependent runs.}
        - restart-verify selector [str] --from [int]  {Restart a session's verify steps from an index onwards.}
        - reviews selector [str]  {The reviews this session's branch participates in.}
        - set-status selector [str] state [str]  {Manually override a session's status.}
        - show selector [str]  {Show a single session's detail.}
        - terminal selector [str] --mode [open|readonly]  {Print the command to resume a session's conversation locally.}
        - worktree selector [str]  {Show the worktree/project a session is using.}
    - show  {Print machine-readable views of ralphus itself.}
        - help-map  {Print the full CLI command surface as an alphabetized, indented tree (for onboarding an AI agent) (RAL-110).}
    - status run_id [str, optional] --concurrency  {Show run status from the daemon.}
    - submit file [str, one or more] --activate --hold --label [str] --no-validate --wait (subagent)  {Submit one or more task TOML files to the daemon.}
    - task  {Task-authoring helpers and task-node inspection.}
        - edit selector [str] --name [str] --project [str]  {Edit a task node's name/project.}
        - restart-verify selector [str] --from [int]  {Restart a task's verify steps from an index onwards.}
        - set-status selector [str] state [str]  {Manually override a task's status.}
        - show selector [str]  {Show a single task node's detail.}
        - show-tutor  {Print the Task TOML schema reference and worked examples.}
    - validate file [path, one or more]  {Validate one or more task TOML files.}
    - verify  {Inspect and act on verify steps.}
        - restart selector [str]  {Restart this verify step (and any later ones in its scope).}
        - set-status selector [str] state [str]  {Manually override a verify step's status.}
        - show selector [str]  {Show a single verify step's detail.}
```
<!-- END GENERATED HELP-MAP (RAL-110) -->
