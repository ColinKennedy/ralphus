# ralphus CLI reference

`ralphus` is a thin, stateless HTTP client over the daemon API
(`docs/daemon-api.md`) — every subcommand below maps to one or more daemon
endpoints. See `CLI_PARITY_PLAN.local.md` for the design history.

## Global flags

Global flags work anywhere before a bare `--`: both
`ralphus --daemon-url http://127.0.0.1:7890 status` and
`ralphus status --daemon-url http://127.0.0.1:7890` are equivalent. Tokens
after `--` belong to the downstream command and are never interpreted as
Ralphus flags.

- `--daemon-url URL` — base URL of the daemon (default `http://127.0.0.1:7890`,
  or `$RALPHUS_DAEMON_URL`).
- `$RALPHUS_DAEMON_TIMEOUT` — per-request HTTP timeout in seconds (default `60`).
  The daemon's HTTP loop is single-threaded, and a mutating request like `submit`
  can involve TOML validation, project-registry lookups, and git calls during
  review derivation, so a busy daemon can take longer than a health check to
  respond. No CLI flag yet — env var only.
- `--json` — emit raw daemon JSON instead of human-readable text. Both
  `ralphus --json status` and `ralphus status --json` work identically.
- `-h`, `--help` — print detailed help for the deepest recognized command and
  exit successfully. Help takes precedence over missing arguments, invalid
  flag values, unknown trailing tokens, global flags, and normal command work.
  Like other Ralphus flags it is not intercepted after a bare `--`.
- `--version` — print the CLI version and exit.

The companion executables follow the same help rule: `ralphus-runner`,
`ralphus-daemon`, and `ralphus-librarian` accept `--help`/`-h` at the root and
after each explicit command. For example, `ralphus-runner send --help`,
`ralphus-daemon stop --help`, and `ralphus-librarian serve --help` all print
detailed usage and exit before doing any command work.

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
(`cli/src/selector.rs`). Anywhere this reference writes `<selector>`,
`<squad_id>`, or a queue item path, the **ralphus URI** form below is accepted
too — including the bare `squad_id` positionals of the `squad *` family, `status`,
`graph`, and `queue set-status`/`reorder`/`set-position`.

### The ralphus URI scheme (RAL-188)

The self-describing form ralphus itself **produces** — every `show` command
prints the exact URI addressing what it just displayed, as its first `uri`
field:

```
ralphus:/SQUAD[<label>]                                                      ?id=<squad_id>
ralphus:/SQUAD[<label>]/TASK[<name>]                                         ?id=<squad_id>
ralphus:/SQUAD[<label>]/TASK[<name>]/PROOF[<name-or-~index>]                 ?id=<squad_id>
ralphus:/SQUAD[<label>]/TASK[<name>]/CELL[<name>]                            ?id=<squad_id>
ralphus:/SQUAD[<label>]/TASK[<name>]/CELL[<name>]/PROOF[<name-or-~index>]    ?id=<squad_id>
ralphus:/REVIEW[<name>]                                                      ?id=<guardian_id>
ralphus:/REVIEW[<name>]?id=<guardian_id>&combined
ralphus:/REVIEW[<name>]?id=<guardian_id>&worktree=<branch-id-or-name-or-~index>
```

Rules:

- **A segment's contents are the entity's label**, falling back to its id when
  no label is set. Labels are neither unique nor stable.
- **`?id=` is the disambiguator.** Formally optional, always emitted by
  anything ralphus produces, and authoritative when present — it wins over a
  stale or renamed label. An ambiguous label with **no** `?id=` exits 2 with
  the list of candidates; it is never silently resolved to the most recent.
- **A positional index carries a `~` sigil** — `PROOF[~0]`, `?worktree=~2`.
  A bare `PROOF[0]` addresses the step literally *named* `0`. This is how an
  anonymous proof step (one with no author-supplied `id`) is addressed. `~` is
  an RFC 3986 **unreserved** character, deliberately: the sigil was originally
  `#`, which is the fragment delimiter, so a URI carrying one was truncated
  anywhere it met a real URL parser (a browser address bar, the board's own
  `location.hash`).
- **`?worktree=` names one stacked branch** by its label (the feature branch
  name the Reviews UI lists it under, e.g. `?worktree=RAL-188-uri-scheme`), by
  its stable `branch-000000000278` id, or by `~<position>`. A label containing
  a `/` is percent-encoded like any other (`?worktree=feature%2Fa`). Use
  `&combined` instead for the review's combined worktree.
- **`[`, `]` and `/` are percent-encoded inside a label**, so a review named
  `RAL-174/175 batch` is written `REVIEW[RAL-174%2F175 batch]`. Spaces stay
  raw. A hand-written literal `/` inside brackets also parses, because
  balanced `[...]` groups are extracted before the path is split.
- The `ralphus:` prefix is a **string format**, not an OS-registered protocol
  handler. The bare `SQUAD[...]` shorthand is accepted too.

Names resolve against the daemon; `GET /api/resolve?uri=...` does the same
translation for non-CLI consumers (see `docs/daemon-api.md`).

### Legacy selector form (still accepted)

| Selector | Addresses |
|---|---|
| `<squad_id>` | a squad |
| `<squad_id>/<task>` | a task node (`task` = index or name) |
| `<squad_id>/<task>/<cell>` | a cell (`cell` = index or name) |
| `<squad_id>/<task>/proof/<i>` | a task-level proof step |
| `<squad_id>/<task>/<cell>/proof/<i>` | a cell-level proof step |
| `<guardian_id>` or `@<name>` | a review |
| `<guardian_id>~<pos-or-branch>` or `@<name>~<pos-or-branch>` | a review branch |
| `<guardian_id>#<pos-or-branch>` or `@<name>#<pos-or-branch>` | a review branch (older spelling; still parsed) |

This form is an accepted alias only — nothing produces it any more. The two
differ in exactly one resolution rule: here a **bare integer is a position**,
whereas in the URI form a position always carries the `~` sigil and a bare
token is always a name. The branch separator was originally `#` for the same
reason the URI sigil was; both spellings still parse, and splitting happens at
whichever appears **first**, so a branch name containing the other one still
splits where you meant it to.

A name segment that matches more than one candidate, or none, exits 2 with the
list of candidates.

## Core

| Command | What |
|---|---|
| `validate <file>` | Validate a task TOML file offline (no daemon needed if `ralphus-daemon` is on PATH) |
| `submit <file...\|dir\|glob\|-> [--label] [--hold] [--activate] [--wait] [--no-validate]` | Submit task TOML. See [Submit](#submit) below |
| `author [--goal\|--prompt-file] [--verify] [--review] [--hold] [--dry-run] ...` | Generate, validate, and submit a TOML from a plain-language goal |
| `status [squad_id] [--concurrency]` | Show one squad, or list all squads; `--concurrency` shows scheduler load instead |
| `resources` | Per-task CPU/RAM/GPU for running cells |
| `graph [squad_id] [--global] [--all] [--format ascii\|dot]` | Render the task-order dependency graph |
| `get <selector> [field.path]` | jq-lite field query over any entity's JSON view |
| `clear [--all\|--status STATES] [--keep-temporary] [--yes]` | Bulk-delete tasks and reviews |
| `check health [--enable-developer-checks]` | System/environment health check |
| `completion bash` | Print a bash tab-completion script |
| `configuration show [--no-local]` | Show sourced `.ralphus.toml` files and resolved values |
| `task show-tutor` | Print the Task TOML schema reference |
| `queue list [--all]` / `reorder <paths...>` / `set-position <paths...> --to N [--relative]` / `set-status <path> <state>` | Inspect/reorder the squad queue by priority |
| `initialize git [--path]` | Enable git rerere in a repository |

### Submit

`submit` accepts one or more sources, each of which may be a `.toml` file
path, a directory (expands to every `*.toml` file in it, non-recursive), a
glob pattern (shell-quote it so `ralphus` expands it, not your shell), or `-`
for stdin. Explicit file paths combine into **one** squad (so a shared
`ralphus:new-review/<key>` folds into a single review); a directory or glob
submits **each matching file as its own separate squad** instead (one failure
aborts the remaining batch).

- `--hold` stages as `queued` instead of `pending`.
- `--activate` forces `--hold` internally, submits, then immediately calls
  `squad activate` — "submit held then promote" in one step.
- `--wait` polls each submitted squad and prints its state until terminal;
  exits 0 only if the squad reached `done` (1 for `failed`/`cancelled`).
- `submit` **validates before submitting by default** (a client-side
  `POST /api/squads/validate` call) and prints the full per-line
  `error [line N]: message` report instead of submitting if it fails. In a
  batch (directory/glob), each file is validated right before its own
  submit, so one invalid file blocks the rest of the batch. `--no-validate`
  skips this pass and submits directly — the daemon still rejects invalid
  TOML server-side either way, so this doesn't let anything through; it only
  trades the fuller pre-submit report for the daemon's single summary error
  message. To validate without submitting at all, use `ralphus validate`.

## squad

| Command | Endpoint |
|---|---|
| `squad list [--status] [--name] [--sort]` | `GET /api/tasks` (server-side filtered/sorted) |
| `squad show <squad_id>` | `GET /api/squads/{id}` |
| `squad logs <squad_id>` | `GET /api/squads/{id}/logs` |
| `squad set-status <squad_id> <state>` | `POST /api/squads/{id}/set-status` |
| `squad restart <squad_id>` | `POST /api/squads/{id}/restart` |
| `squad retry <squad_id>` | `POST /api/squads/{id}/retry` |
| `squad activate <squad_id>` | `POST /api/squads/{id}/activate` |
| `squad cancel <squad_id>` | `POST /api/squads/{id}/cancel` |
| `squad delete <squad_id> [--yes]` | `DELETE /api/squads/{id}` |
| `squad rename <squad_id> <label>` | `POST /api/squads/{id}/edit` |
| `squad edit <squad_id> [--label]` | `POST /api/squads/{id}/edit` |

## task

| Command | What |
|---|---|
| `task show <selector>` | Task node detail |
| `task set-status <selector> <state>` | Override a task's status |
| `task restart-proof <selector> --from <i>` | Restart task-level proof steps from index `i` |
| `task edit <selector> [--name] [--project]` | Edit a task node's name/project |

## cell

| Command | What |
|---|---|
| `cell show <selector>` | Cell detail (incl. tokens, cost, agent, model, cwd) |
| `cell worktree <selector>` | The worktree/project a cell is using |
| `cell reviews <selector>` | Reviews this cell's branch participates in |
| `cell set-status <selector> <state>` | Override a cell's status |
| `cell restart <selector>` | Restart a cell and its downstream |
| `cell restart-proof <selector> --from <i>` | Restart cell-level proof steps from index `i` |
| `cell edit <selector> [--cwd] [--agent] [--model] [--prompt] [--command] [--system-prompt]` | Edit a cell's fields |
| `cell terminal <selector> [--mode open\|readonly]` | Print the `claude --resume` command + cwd (does **not** ask the daemon to spawn a terminal — see below) |

## proof

| Command | What |
|---|---|
| `proof show <selector>` | Proof step detail (kind, state, spec, output) |
| `proof set-status <selector> <state>` | Override a proof step's status |
| `proof restart <selector>` | Restart a proof step (dispatches to the task- or cell-scoped restart endpoint) |

## review

| Command | What |
|---|---|
| `review list [--status] [--pr-ready]` | List reviews (`--pr-ready`: only fully-rebased `in_review`/`approved` reviews with no failed branches — the candidates `review pr submit` cares about) |
| `review show <selector>` | Review detail, incl. `ready`/`merge_progress`/`summary_state` |
| `review logs <selector>` | State-transition audit log |
| `review status <selector>` | Per-branch `merge_status`/`ready`/detail + a summary verdict — "is this review ready?" |
| `review worktrees <selector>` | Worktrees/branches this review consumes, with source cell |
| `review create <name> <base_branch> <git_root> [--checks] [--skip-checks] [--skip-worktrees] [--review-type]` | Create a review |
| `review rename <selector> <name>` | Rename |
| `review cancel <selector>` | Cancel |
| `review reopen <selector>` | Reopen a cancelled review, immediately staging in whatever branches are already ready |
| `review delete <selector> [--yes]` | Delete + purge worktrees |
| `review settings <selector> [--skip-checks] [--skip-worktrees] [--resolver-agent] [--resolver-model] [--base-branch]` | Update opt-out settings |
| `review add-branch <selector> <branch>` | Add a branch |
| `review reorder <selector> <b1,b2,...> [--disable b,b] [--enable b,b]` | Reorder branches + rebase, atomically |
| `review branch enable\|disable <selector#branch>` | Enable/disable one branch + rebase |
| `review merge <selector>` | Start/continue the stacked rebase |
| `review sync-pr <selector>` | Check the forge (GitHub or GitLab) for a stack reorder made outside ralphus and apply it if found (RAL-273) |
| `review restart-merge <selector>` | Cancel an in-progress rebase, start fresh |
| `review stop-merge <selector>` | Stop an in-progress rebase, leaving the review resumable (not cancelled) |
| `review force-start <selector>` | Disable not-yet-done branches, merge immediately |
| `review approve <selector>` | Approve an in_review review |
| `review feedback <selector#branch> <text> [--author name]` | Post feedback on a branch, optionally attributed to a different registered user than the one submitting it (RAL-379); defaults to the submitter when omitted |
| `review dismiss-reenable <selector#branch>` | Dismiss the "can re-enable" notice |
| `review move-branch <selector#branch> <to_review>` | Move a branch to another review + rebuild both (RAL-118) |
| `review upstream list <selector>` | Candidate upstream branches |
| `review upstream set <selector> <branch>` | Change upstream branch + rebuild |
| `review checks list <selector>` | List LLM-synthesized manual review-verification commands |
| `review checks run <selector> [--index N...] [--all] [--input NAME=VALUE...]` | Print command(s) + cwd for one/some/all checks (not run by the daemon — see below) |
| `review action list <selector>` | List user-declared `[[review.action]]` hints |
| `review action run <selector> --index N [--input NAME=VALUE...]` | Print the command + cwd for a `command`-kind hint |
| `review pr submit <selector> (--position N \| --combined) [--alias] [--title] [--description] [--allow-unlinked-fork]` | Submit a PR/MR (RAL-117); title/description default to an LLM suggestion. `--allow-unlinked-fork` (RAL-338) downgrades a definite "no forge relationship" fork pre-flight result from a hard error to a logged warning; ignored for a project with no registered fork |
| `review pr list <selector>` | List PRs submitted for a review |
| `review pr show <pr_id>` | Show one PR row |
| `review pr find <forge> <repo> <pr_number>` | Look up the PR row for a forge PR/MR number |
| `review pr update <pr_id> [--pr-number] [--pr-url] [--branch-alias] [--state]` | Mutate the recorded PR mapping (e.g. after a reopen) |
| `review pr comments <pr_id>` | List a PR's comments, flagging which are already actioned |
| `review pr pull-feedback <pr_id>` | Action a PR's un-actioned feedback into the owning worktree |
| `review pr pull-from-pr <pr_id>` | Pull a reviewer's commits pushed directly to the PR branch into the owning worktree, resolving conflicts and restacking downstream branches (RAL-190) |

### Why some commands print instead of act

`cell terminal`, `review checks run`, and `review action run` deliberately
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

## project

| Command | What |
|---|---|
| `project git --path <p> --name <n> [--url] [--clear-url] [--description] [--match-pr-branch-name]` | Register/update a project's git info |
| `project list [--short]` | List registered projects |
| `project get <name>` | One project's detail |
| `project fork add <project> --url <url> [--user] [--remote-name] [--owner]` | [Register a fork](fork-workflows.md) (RAL-338), optionally scoped to one user (defaults to the project-wide fallback row when `--user` is omitted) |
| `project fork list [<project>] [--user] [--short]` | List registered forks, optionally scoped to one project and/or filtered to one user |
| `project fork set <project> [--user] [--url] [--remote-name] [--owner]` | Update fields on an existing fork registration |
| `project fork remove <project> [--user]` | Remove a fork registration |

See [`fork-workflows.md`](fork-workflows.md) for the fork registration
surface's topology, promotion, and setup guidance, and
[`daemon-api.md`](daemon-api.md#fork-registration-ral-338) for the HTTP wire
shapes. `review pr submit` gains `--allow-unlinked-fork` (see below) once a
project has a registered fork.

## show

| Command | What |
|---|---|
| `show help-map` | Print the full CLI command surface as an alphabetized, indented tree (see [Machine-readable help-map](#machine-readable-help-map-ral-110) below) |

## quick-start

Three independent command families (RAL-166/RAL-241), each with a
`claude-code`, `codex`, and `pi` entrypoint:

| Command | What |
|---|---|
| `quick-start manager claude-code [--command] [--shell] [-- ARGS...]` | Launch Claude Code primed with the full CLI help-map as a system prompt, so it can orchestrate `ralphus` unsupervised. See [quick-start manager](#quick-start-manager) below |
| `quick-start manager codex [--command] [--shell] [-- ARGS...]` | Launch Codex primed with the same help-map system prompt, so it can orchestrate `ralphus` unsupervised. See [quick-start manager](#quick-start-manager) below |
| `quick-start manager pi [--command] [--shell] [-- ARGS...]` | Launch Pi primed with the same manager role and help-map. See [quick-start manager](#quick-start-manager) below |
| `quick-start reviewer claude-code [TARGET] [--command] [--shell] [-- ARGS...]` | Launch Claude Code primed to act as a reviewer on an existing review. See [quick-start reviewer](#quick-start-reviewer) below |
| `quick-start reviewer codex [TARGET] [--command] [--shell] [-- ARGS...]` | Launch Codex primed to act as a reviewer on an existing review. See [quick-start reviewer](#quick-start-reviewer) below |
| `quick-start reviewer pi [TARGET] [--command] [--shell] [-- ARGS...]` | Launch Pi primed to act as a reviewer on an existing review. See [quick-start reviewer](#quick-start-reviewer) below |
| `quick-start watcher claude-code [--command] [--shell] [-- ARGS...]` | Launch Claude Code primed to poll the escalation mailbox after every user turn. See [quick-start watcher](#quick-start-watcher) below |
| `quick-start watcher codex [--command] [--shell] [-- ARGS...]` | Launch Codex primed to poll the escalation mailbox after every user turn. See [quick-start watcher](#quick-start-watcher) below |
| `quick-start watcher pi [--command] [--shell] [-- ARGS...]` | Launch Pi primed to poll the escalation mailbox after every user turn. See [quick-start watcher](#quick-start-watcher) below |

There is no compatibility alias for the old `quick-start claude-code` shape —
it is fully replaced by `quick-start manager claude-code`.

### quick-start manager

Both `manager` entrypoints inject the same "orchestrate ralphus itself"
system prompt — the full help-map (same tree `show help-map` prints) plus the
`SUBAGENT_NOTE`/`PROJECT_LOOKUP_NOTE`/`SUBMIT_VALIDATE_NOTE`/
`SUBMIT_REVIEW_NOTE` guidance — only the delivery mechanism differs:

- **`quick-start manager claude-code`** writes the prompt to a throwaway temp
  file and launches `claude --dangerously-skip-permissions
  --append-system-prompt-file <tempfile> ...`, so the file's contents become
  Claude Code's system prompt via the dedicated file-based flag (the path is
  its own argument value, not embedded in a text value, so it can't be
  truncated by a space in the path). If the forwarded `--` args include their
  own `--append-system-prompt-file`, its contents are read and folded into
  ralphus's own temp file instead — ralphus's context first, then a
  disclaimer, then the user's — rather than forwarding a second, separate
  occurrence of the flag. The `claude` launch command resolves, in order:
  `--command` (this invocation only), then `$RALPHUS_CLAUDE_COMMAND`, then
  the bare `claude` on PATH.
- **`quick-start manager codex`** launches an interactive `codex` session
  with the prompt injected via `-c developer_instructions=...` — Codex has no
  file-based system-prompt flag; this `-c` override is the closest analog
  (see `ralphus.runner.codex_backend`'s module docstring) and must precede
  any subcommand to be recognized, which is why this quick-start launches
  the bare interactive TUI rather than `codex exec`. The `codex` launch
  command resolves, in order: `--command` (this invocation only), then
  `$RALPHUS_CODEX_COMMAND`, then the bare `codex` on PATH.

Common to both:

- Args after a literal `--` are forwarded verbatim to the underlying
  harness, e.g. `ralphus quick-start manager claude-code -- --mode auto` or
  `ralphus quick-start manager codex -- --model gpt-5-codex`.
- `$RALPHUS_CLAUDE_COMMAND`/`$RALPHUS_CODEX_COMMAND` are the same env vars the
  `claude-code`/`codex` agent backends use
  (`ralphus.runner.claude_code_backend`/`ralphus.runner.codex_backend`) —
  one name per harness, everywhere that harness's executable is resolved.

#### Agent Profiles (RAL-243)

Custom backend routing belongs in `.ralphus.toml`, not in task TOML:

```toml
[agent.profiles.openrouter-deepseek]
backend = "codex"
executable = "codex-openrouter"

[agent.profiles.openrouter-deepseek.env]
OPENROUTER_API_KEY = { from_env = "OPENROUTER_API_KEY" }
```

Then a task or cell selects it through the existing field:

```toml
agent = "openrouter-deepseek"
```

Rules:

- Profile names must not collide with reserved built-in backends (`claude`, `anthropic`, `ollama`, `claude-code`, `codex`, `raw`, plus the CLI aliases).
- `backend = "raw"` is the explicit generic external-executable backend and requires `executable`.
- `executable` is only valid with `claude-code`, `codex`, or `raw`; it is rejected for native backends (`claude`, `anthropic`, `ollama`).
- Profile env values may be literal strings or `{ from_env = "VAR" }`; indirection is resolved in the daemon's own OS environment, so secrets never appear in task TOML or HTTP request/response bodies.
- If a cell resolves to a custom agent profile, do not also set `model`. Current v1 rule: `if you're using a custom agent profile, you can't also set model`.
- The old implicit fallback from an unknown `agent` name to a generic harness executable is gone. Use a named profile instead.

#### What `--command` accepts (RAL-189)

The launch command value takes three shapes, all of which behave the way the
same text would typed straight into a terminal (`ralphus.shellcmd`):

| Shape | Example | How it runs |
|---|---|---|
| A bare executable or path | `claude`, `"C:\Program Files\claude\claude.exe"` | Exec'd directly — ralphus's own arguments are appended as real argv entries, so no shell and no quoting can go wrong |
| A single-name script | `my-claude.ps1`, `launch-claude.sh` | Resolved to a full path, then run through the target shell — the OS can't `exec` a `.ps1`/non-`+x` script itself |
| A raw shell command line | `cd /foo/bar ; claude`, `python some_script.py -- super-claude` | Passed to the target shell verbatim, with ralphus's arguments quoted for that shell and appended |

Which shape applies is decided by the RAL-110 heuristic in
[`check health`](#core) above: no spaces (or fully wrapped in quotes) means a
bare name/path, anything else is a raw command line. **A bare path containing
a space (e.g. a Windows install under `C:\Program Files\...`) must be wrapped
in quotes** (`"C:\Program Files\claude\claude.exe"`) or it is misdetected as a
raw command line (RAL-110 Q5).

Single-name lookup mirrors the target shell's own: on Windows, the current
directory then `PATH`, trying each `%PATHEXT%` suffix (so a bare `my-claude`
finds `my-claude.cmd`); on POSIX, `PATH` only — a script in the working
directory must be spelled `./my-claude.sh` there, exactly as at a prompt. A
name that resolves to nothing is still handed to the OS directly, so a genuine
typo surfaces as one clean `could not launch ...` error.

#### `--shell` — which shell interprets a `--command`

`--shell` picks the shell used for the two shapes that need one (a script, or
a raw command line); it is ignored for a directly-executable program. Accepted
values: `auto` (default), `powershell`, `pwsh`, `cmd`, `bash`, `sh`, `zsh`,
`fish`.

`auto` means **the shell that launched `ralphus`**, detected by walking the
real process ancestry (Toolhelp32 on Windows, `/proc` on Linux) and falling
back to environment heuristics (`$SHELL`, `%COMSPEC%`,
`$POWERSHELL_DISTRIBUTION_CHANNEL`, `%PSModulePath%`) and then the platform
default. So a `;` typed at a PowerShell prompt keeps meaning what PowerShell
says it means, rather than being reinterpreted by a hardcoded `cmd /C`.

Name a shell explicitly to author a command *for a different shell* than the
one you're sitting in:

```bash
# From a PowerShell prompt, but the command is written for bash:
ralphus quick-start manager claude-code --shell bash --command 'cd /foo/bar ; claude'

# A PowerShell script, resolved off PATH and run by PowerShell:
ralphus quick-start manager claude-code --command my-claude.ps1

# A raw command line with its own arguments:
ralphus quick-start reviewer codex @myreview --command 'python some_script.py -- super-claude'
```

`$RALPHUS_SHELL` overrides what `auto` detects (useful in a wrapper script or
a CI job where ancestry detection has nothing meaningful to find); an explicit
`--shell` overrides `$RALPHUS_SHELL` in turn. An unrecognized `$RALPHUS_SHELL`
value is ignored rather than fatal — an unrecognized `--shell` value is
rejected by argparse.

### quick-start reviewer

Both `reviewer` entrypoints inject a review-focused system prompt (not a copy
of the manager prompt): it frames the harness as operating on an *existing*
review through the `ralphus review ...` CLI surface — branch feedback,
combined/global review feedback, merge/rebase start or restart, branch
enable/disable, base-branch changes, manual checks, and action hints — rather
than orchestrating new ralphus tasks. The full help-map is still appended for
reference. Delivery mechanism mirrors the matching `manager` entrypoint
(temp-file for `claude-code`, `-c developer_instructions=...` for `codex`).

- An optional `TARGET` positional seeds the initial review context: a
  guardian id, `@name`, or a review-resolving URL (the `#/reviews/<id>` shape
  `board.html` puts in the address bar, e.g.
  `http://127.0.0.1:7474/#/reviews/guardian-000000000042`). A URL with no
  recognizable `.../reviews/<id>` segment falls back to being treated as a
  literal selector, best-effort. TARGET is optional — omit it to start with
  no fixed review in mind.
- The agent can switch to a different review at any point in the
  conversation (`review list`, then `review show <selector>`) without being
  relaunched — TARGET only seeds where the session starts.
- Remote-state caution is built into the prompt: the harness must not assume
  the review's code lives on the machine it's running on (the daemon, not
  this host, is the source of truth), and any read-only inspection of review
  code should go through the CLI's own `--json` views and printed
  check/action commands rather than an assumed local checkout. All
  write-oriented review actions must go through an explicit `ralphus review
  ...` subcommand, never ad-hoc shell mutation in an inspected worktree.
- RAL-375: after every user turn the injected prompt also runs `ralphus
  mailbox check --category review` and shows its output verbatim, before the
  agent would otherwise go idle. This drains PR/CI-watch notices: once
  `review feedback` pushes a commit onto a PR-linked branch, the daemon
  watches that PR's CI/CD and merge/rebase-blocker status in the background
  and reports here on failure, with the PR URL, failing job URL, impacted
  review worktree, and a trimmed log excerpt, asking whether to fix it
  immediately in a subagent. The `--category` filter keeps this drain scoped
  to review notices — see [mailbox](#mailbox) below.

### quick-start watcher

RAL-241, poll-only scope: a mailbox-polling supervisor session. Both
entrypoints register (or reuse a locally persisted) mailbox `client_id` with
the daemon (`POST /api/mailbox/register`) before launch, then inject a system
prompt instructing the agent to run `ralphus mailbox check` (no `--category`
filter, unlike `quick-start reviewer`'s `--category review` — RAL-375) after
every user turn and show its output verbatim — `urgent` messages must be read
and acted on before continuing, `high` before the agent would otherwise go
idle, `normal` is informational. This includes `review`-category PR/CI-watch
notices, so a watcher session surfaces them too even when the user isn't in
reviewer mode. Delivery mechanism mirrors the matching `manager`/`reviewer`
entrypoints (temp-file for `claude-code`, `-c developer_instructions=...` for
`codex`); the full help-map is still appended for reference, since the
watcher may need to inspect/act on whatever the escalation is about.

Direct-push delivery into a live tmux-tracked session (rather than this
turn-boundary poll) is out of scope here — see the ticket's Q&A.

## mailbox

RAL-241, poll-only scope: the escalation mailbox client.

| Command | What |
|---|---|
| `mailbox check [--priority urgent\|high\|normal] [--category <name>]` | List this client's unread messages, print them, then drain (mark read) exactly the ones printed |

`mailbox check` mints (or reuses) a `client_id` the same way `quick-start
watcher` does, so a stray manual run before ever launching a watcher session
still works. Not on the `--read-only` safety list — it mutates drain state as
a side effect even though it takes no other flags. `--category` restricts to
one broad message classification (e.g. `review`, RAL-375, for PR/CI-watch
notices); omit it to drain every category, which is what `quick-start
watcher` does while `quick-start reviewer` defaults to `--category review`.

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
authoring agent through it; every `quick-start manager|reviewer
claude-code|codex` entrypoint (RAL-166) launches an entire separate
`claude`/`codex` process, which the driving agent should never re-launch on
itself.

A hand-picked subset of commands (`ralphus.helpmap._SUBAGENT_PATHS`) carries
an inline `(subagent)` tag: `review`, `submit`, `clear`, and `check health`
today — each is slow/blocking, destructive, or does multi-step
subprocess/filesystem work whose result isn't needed synchronously. Cheap,
frequently-polled reads (`status`, `get`, ...) and tight edit-loop commands
(`validate`) are deliberately left untagged. Wherever this tree is shown to
an AI agent — `show help-map`, every `quick-start manager|reviewer
...`'s injected system prompt, `ralphus-help-map`'s own stdout — it's preceded by
`ralphus.helpmap`'s note block, including `SUBAGENT_NOTE` (what the tag
means), `READ_ONLY_NOTE`, `PROJECT_LOOKUP_NOTE`, `SUBMIT_VALIDATE_NOTE`,
`SUBMIT_REVIEW_NOTE`, and `JSON_NOTE`. `SUBAGENT_NOTE` specifically explains
the tag: invoke that command from inside a subagent (e.g. Claude Code's Task
tool) rather than directly in the driving agent's main context.

A second, non-mutating subset of commands (`ralphus.helpmap._READ_ONLY_SAFE_PATHS`)
carries a `(read-only-safe)` tag — these are the commands a `quick-start
manager --read-only` / `quick-start reviewer --read-only` session may still
use; see `READ_ONLY_NOTE`.

<!-- BEGIN GENERATED HELP-MAP (RAL-110) -->
```
- ralphus --daemon-url [url] --json --version  {Submit and manage autonomous agent tasks against the ralphus daemon.}
    - agent  {Inspect agent backends ralphus can run.}
        - (read-only-safe) list  {List supported agent backends and the models each is allowed to run.}
    - cartographer --ascending --cell [str] --entity [str] --for [str] --guardian [str] --level [str] --limit [integer] --offset [integer] --q [str] --scope [str] --source [str] --squad [str] --task [str]  {Query the structured Cartographer event log (RAL-98/RAL-155).}
    - cell  {Inspect and act on cells.}
        - edit selector [str] --agent [name] --auto-compact-threshold [tokens] --command [cmd] --cwd [path] --maximum-tool-output-tokens [tokens] --model [name] --prompt [text] --system-prompt [text]  {Edit a cell's fields.}
        - (read-only-safe) env selector [str] --scope [cell|proof]  {List a cell's resolved environment variables, read-only (RAL-324); --scope proof shows what its own proof steps inherit.}
        - open-agent selector [str]  {Open the real interactive agent in a new terminal -- while running, cleanly detaches the cell first (RAL-288); while finished, resumes it the old way.}
        - remote-terminal selector [str]  {Attach an interactive terminal to a remote cell's resumed Claude Code session over the daemon's WebSocket relay (RAL-355).}
        - restart selector [str]  {Restart a cell (and its downstream), dirtying dependent squads.}
        - restart-proof selector [str] --from [index]  {Restart a cell's proof steps from an index onwards.}
        - resume-automation selector [str]  {Hand a detached cell back to unattended execution, continuing the exact same agent conversation (RAL-288).}
        - (read-only-safe) reviews selector [str]  {The reviews this cell's branch participates in.}
        - set-status selector [str] state [str]  {Manually override a cell's status.}
        - (read-only-safe) show selector [str]  {Show a single cell's detail.}
        - (read-only-safe) terminal selector [str] --mode [open|readonly]  {Print the command to resume a cell's conversation locally.}
        - (read-only-safe) worktree selector [str]  {Show the worktree/project a cell is using.}
    - check  {System and environment checks.}
        - (read-only-safe) health --all-remotes --enable-developer-checks --json (subagent)  {Check the local ralphus setup (daemon, git, runner, ollama). --all-remotes also checks every configured [machine.targets.*] entry (RAL-355 Phase 9).}
    - clear --all --keep-temporary --status [states] --yes (subagent)  {Delete tasks and reviews from the daemon.}
    - (read-only-safe) completion  {Print a shell tab-completion script. (Rust port: not yet implemented -- prints a placeholder message; Python's `shell` argument is not read.)}
    - (read-only-safe) configuration  {Show sourced .ralphus.toml files and resolved values. (Python's separate `configuration show` subcommand is flattened into this bare command in the Rust port; --no-local is not yet ported.)}
    - (read-only-safe) get selector [str] field [str, optional]  {Query one field from any entity's JSON view (jq-lite).}
    - (read-only-safe) graph squad_id [str, optional] --all --dot  {Render the task-order dependency graph. (Rust port simplifies Python's --global/--format ascii|dot choice to plain --dot/--all boolean flags.)}
    - (read-only-safe) history selector [str]  {Show a cell/proof step's tmux history (one-shot snapshot; Python's --live tailing and --wait-until-valid are not yet ported).}
    - initialize  {One-time local setup helpers for a repository.}
        - git --path [path]  {Enable git rerere in a repo so review rebases replay conflict resolutions.}
    - (read-only-safe) license  {Print the embedded LICENSE text decoded from the binary's obfuscated copy.}
    - (read-only-safe) listen selector [str] --timeout [seconds] --until [status]  {Block until a squad/task/cell/proof/review/review-worktree reaches a status.}
    - machine  {Register and inspect machine providers remote work runs on.}
        - cleanup machine [str] --branch [name] --project [name]  {Tear down one project's provisioned workspace on a machine provider -- the whole project directory, or just --branch's worktree (RAL-201, reshaped by RAL-355 Phase 2).}
        - (read-only-safe) get scheme [str]  {Show one registered machine provider by exact scheme.}
        - (read-only-safe) list  {List every registered machine provider, plus built-in schemes.}
        - register --arg [value...] --channel --description [text] --program [path] --scheme [name]  {Register a provider program a task's 'machine' field can reference.}
        - remove scheme [str]  {Remove a registered machine provider.}
    - mailbox  {Drain the escalation mailbox (RAL-241): failed/stalled work the daemon flagged for attention. Also personal watches and notification preferences layered over the same mailbox (RAL-320).}
        - check --category [name] --priority [urgent|high|normal]  {Drain unread escalation mailbox messages and print them (RAL-241). --category restricts to one message category, e.g. "review" (RAL-375).}
        - (read-only-safe) personal --priority [urgent|high|normal] --unread --user [name]  {List the acting user's personal mailbox messages, filtered through their watches (RAL-320).}
        - personal-drain --id [id...] --user [name]  {Mark personal mailbox messages read; omit --id to drain every unread message (RAL-320).}
        - (read-only-safe) preferences --user [name]  {Show a user's notification preferences: automatic creator watches and default notify tiers.}
        - set-preferences --auto-watch --no-auto-watch --tier [urgent|high|normal...] --user [name]  {Set a user's automatic-watch and default notification-tier preferences; requires exactly one of --auto-watch/--no-auto-watch (RAL-320).}
        - unwatch entity_uri [str] --user [name]  {Stop watching an entity (RAL-343).}
        - watch entity_uri [str] --tier [urgent|high|normal...] --user [name]  {Watch an entity so its notifications reach the personal mailbox; re-watching updates the notification tiers in place (RAL-343).}
        - (read-only-safe) watches --user [name]  {List the acting user's watches (RAL-343).}
    - project  {Register and inspect projects known to the daemon.}
        - fork  {Manage per-project, per-user fork registrations for fork-based stacked PR routing.}
            - add project [str] --owner [owner] --remote-name [name] --url [url] --user [name]  {Register a fork for a project, optionally scoped to one user (defaults to the project-wide fallback row when --user is omitted).}
            - (read-only-safe) list project [str, optional] --short --user [name]  {List registered forks, optionally scoped to one project and/or filtered to one user.}
            - remove project [str] --user [name]  {Remove a fork registration (defaults to the project-wide fallback row when --user is omitted).}
            - set project [str] --owner [owner] --remote-name [name] --url [url] --user [name]  {Update fields on an existing fork registration (defaults to the project-wide fallback row when --user is omitted).}
        - (read-only-safe) get name [str]  {Show one registered project's details by exact name.}
        - git --clear-url --description [text] --match-pr-branch-name/--no-match-pr-branch-name --name [name] --path [path] --url [url]  {Register a git repository as a project the daemon can resolve placeholder cell cwds against.}
        - (read-only-safe) list --short  {List every project registered with the daemon.}
    - proof  {Inspect and act on proof steps.}
        - edit selector [str] --maximum-tool-output-tokens [tokens] --model [name]  {Edit a proof step's model/tool-output-cap overrides.}
        - (read-only-safe) env selector [str]  {List a proof step's resolved environment variables, read-only (RAL-324); values of names registered in the Secrets tab are masked.}
        - restart selector [str]  {Restart this proof step (and any later ones in its scope).}
        - set-status selector [str] state [str]  {Manually override a proof step's status.}
        - (read-only-safe) show selector [str]  {Show a single proof step's detail.}
    - queue  {Inspect and reorder the squad queue by priority.}
        - (read-only-safe) list --all  {List queued work items (ready-to-run by default).}
        - reorder paths [str...]  {Set the queue order to the given item paths (dependency-repaired).}
        - set-position paths [str...] --relative --to [integer]  {Move item(s) to an absolute index or a relative offset.}
        - set-status path [str] state [str]  {Set a squad/task/cell/proof status (e.g. ignored) by item path or squad id.}
    - (read-only-safe) resources  {Show per-task resource usage (CPU/RAM/GPU).}
    - retry squad_id [str]  {Re-run a squad from scratch (reset to pending). (Rust port: squad-level only; Python's per-selector --environment/--env-file overrides are not yet ported.)}
    - review (subagent)  {Inspect and act on reviews (guardians).}
        - action  {User-declared [[review.action]] test/action hints.}
            - (read-only-safe) list selector [str]  {List the action hints.}
            - (read-only-safe) run selector [str] --index [integer] --input [name=value...]  {Print the command + cwd for a command-kind action hint.}
        - add-branch selector [str] branch [str]  {Add a branch to a review.}
        - approve selector [str]  {Approve a review that is in_review.}
        - branch  {Enable/disable one review branch.}
            - disable selector [str]  {Disable a branch and kick off the rebase.}
            - enable selector [str]  {Enable a branch and kick off the rebase.}
            - terminal selector [str] --mode [open|readonly]  {Print the command to resume a branch's conflict-resolver conversation locally.}
        - build-env selector [str] --clear [key...] --set [key=value...] --unset [key...]  {Set/unset/clear this review's build/check-gate step environment overrides.}
        - cancel selector [str]  {Cancel a review.}
        - checks  {LLM-synthesized manual review-verification commands.}
            - (read-only-safe) list selector [str]  {List the manual checks.}
            - (read-only-safe) run selector [str] --all --index [integer...] --input [name=value...]  {Print the command(s) + cwd to run one/some/all manual checks yourself.}
            - terminal selector [str] --mode [open|readonly]  {Print the command to resume the manual-checks-generation agent conversation locally.}
        - create name [str] base_branch [str] git_root [str] --checks [list] --review-type [label] --skip-auto-build --skip-worktrees  {Create a new review.}
        - delete selector [str] --yes  {Delete a review and its worktrees.}
        - dismiss-reenable selector [str]  {Dismiss the 're-enable' notification for a branch.}
        - (read-only-safe) env selector [str] --scope [build|tests|manual-checks|worktree]  {List a review surface's resolved environment variables, read-only (RAL-324): the auto-build step, the check gates, manual checks, or one branch's review worktree.}
        - feedback selector [str] text [str] --author [name]  {Post feedback on one branch, triggering a resolver re-attempt. --author attributes the feedback to a different registered user than the one submitting it (RAL-379); defaults to the submitter when omitted.}
        - force-start selector [str]  {Disable not-yet-done branches and merge immediately (only while collecting).}
        - (read-only-safe) list --pr-ready --status [statuses]  {List reviews.}
        - (read-only-safe) logs selector [str]  {Show a review's state-transition audit log.}
        - manual-checks-env selector [str] --clear [key...] --set [key=value...] --unset [key...]  {Set/unset/clear this review's manual-checks step environment overrides.}
        - merge selector [str]  {Start (or continue) the stacked rebase.}
        - move-branch selector [str] to_review [str]  {Move a branch to another review, then rebuild both.}
        - pr  {Submit/query pull requests for a review.}
            - (read-only-safe) comments pr_id [str]  {List a PR's comments/notes.}
            - (read-only-safe) find forge [github|gitlab] repo [str] pr_number [integer]  {Look up the ralphus PR row for a forge PR/MR number.}
            - (read-only-safe) list selector [str]  {List PRs submitted for a review.}
            - pull-feedback pr_id [str]  {Action a PR's un-actioned feedback into the owning review worktree.}
            - pull-from-pr pr_id [str]  {Pull a reviewer's commits pushed directly to the PR branch back into the owning review worktree, resolving conflicts and restacking downstream branches (RAL-190).}
            - (read-only-safe) show pr_id [str]  {Show one PR row.}
            - submit selector [str] --alias [name] --allow-unlinked-fork --combined --description [text] --position [integer] --title [text] --use-worktree-branch-name  {Submit a PR/MR for one stacked branch or the combined worktree. --allow-unlinked-fork (RAL-338) downgrades a definite "no forge relationship" fork pre-flight result from a hard error to a logged warning; ignored for a project with no registered fork.}
            - unlink selector [str]  {Bulk-drop every currently open PR row for a review and clear its registered forge PR stack number, so a later submission starts a fresh stack instead of appending to one whose PRs were just unlinked (RAL-317).}
            - update pr_id [str] --branch-alias [name] --pr-number [integer] --pr-url [url] --state [open|merged|closed]  {Mutate the recorded PR mapping, e.g. after a PR is closed and reopened under a new number.}
        - rename selector [str] name [str]  {Rename a review.}
        - reopen selector [str]  {Reopen a cancelled review and immediately stage in whatever branches are already ready, without waiting for the rest.}
        - reorder selector [str] order [str] --disable [names] --enable [names]  {Set the branch order and kick off the rebase.}
        - restart-merge selector [str]  {Cancel an in-progress rebase and start a fresh one.}
        - settings selector [str] --auto-pr-feedback/--no-auto-pr-feedback --auto-submit-pr-stack/--no-auto-submit-pr-stack --base-branch [branch] --match-pr-branch-name/--no-match-pr-branch-name --proof-scope [each_branch|final_branch|nothing] --resolver-agent [name] --resolver-model [name] --separate-pr-branch/--no-separate-pr-branch --skip-auto-build/--no-skip-auto-build --skip-auto-clean/--no-skip-auto-clean --skip-base-updates/--no-skip-base-updates --skip-worktrees/--no-skip-worktrees  {Update per-review opt-out settings.}
        - (read-only-safe) show selector [str]  {Show a single review's detail.}
        - squash selector [str] project [str] --off --on  {Enable/disable squashing one git project's task branches to a single commit each in the review worktree.}
        - (read-only-safe) status selector [str]  {Per-branch readiness + a summary verdict ('is this review ready?').}
        - stop-merge selector [str]  {Stop an in-progress rebase at the next checkpoint, leaving the review resumable instead of cancelled.}
        - sync-pr selector [str]  {Check the forge for a stack reorder made outside ralphus and apply it if found.}
        - upstream  {Inspect/change a review's upstream branch.}
            - (read-only-safe) list selector [str]  {List candidate upstream branches.}
            - set selector [str] branch [str]  {Change the upstream branch.}
        - (read-only-safe) worktrees selector [str]  {The worktrees/branches this review consumes.}
    - show  {Print machine-readable views of ralphus itself.}
        - (read-only-safe) help-map  {Print the full CLI command surface as an alphabetized, indented tree (for onboarding an AI agent).}
    - squad  {Inspect and act on squads.}
        - activate squad_id [str]  {Promote a held (queued) squad to pending.}
        - cancel squad_id [str]  {Cancel a squad.}
        - delete squad_id [str] --yes  {Permanently delete a squad.}
        - edit squad_id [str] --label [text]  {Edit a squad's fields.}
        - (read-only-safe) env squad_id [str]  {List a squad's resolved environment variables, read-only (RAL-324); values of names registered in the Secrets tab are masked.}
        - (read-only-safe) list --name [substring] --sort [date|name] --status [states]  {List squads.}
        - (read-only-safe) logs squad_id [str]  {Show a squad's state-transition audit log.}
        - rename squad_id [str] label [str]  {Rename a squad's label.}
        - restart squad_id [str]  {Restart a whole squad, dirtying every squad that depends on it.}
        - retry squad_id [str]  {Re-run with the same parameters (reset to pending).}
        - set-status squad_id [str] state [str]  {Manually override a squad's status.}
        - (read-only-safe) show squad_id [str]  {Show a single squad's detail.}
        - timeline squad_id [str] --write [path]  {Generate the merged, chronological uber-log-viewer timeline for a squad (RAL-155).}
    - (read-only-safe) status squad_id [str, optional] --concurrency  {Show squad status from the daemon.}
    - submit file [str...] --activate --hold --label [text] --no-validate --wait (subagent)  {Submit one or more task TOML files to the daemon.}
    - task  {Task-authoring helpers and task-node inspection.}
        - edit selector [str] --model [name] --name [name] --project [name]  {Edit a task node's name/project/model.}
        - (read-only-safe) env selector [str] --scope [task|proof]  {List a task's resolved environment variables, read-only (RAL-324); --scope proof shows what its task-scoped proof steps inherit.}
        - restart-proof selector [str] --from [index]  {Restart a task's proof steps from an index onwards.}
        - set-status selector [str] state [str]  {Manually override a task's status.}
        - (read-only-safe) show selector [str]  {Show a single task node's detail.}
    - triage  {Register and inspect Triage types -- the Arbiter subsystem's automatic-review classification categories (RAL-318).}
        - pool  {Inspect and configure Triage auto-review pools (RAL-318).}
            - (read-only-safe) list  {List every Triage pool key with pooled cells and/or a configured count threshold, plus its resolved project name (RAL-318).}
            - threshold project [str] triage_type [str] --clear --threshold [integer]  {Set (or --clear) the count threshold for a (project, triage_type) pool -- once it holds this many cells, it drains into a fresh review (RAL-318).}
        - type  {Register and inspect Triage types (RAL-318).}
            - deregister name [str]  {Remove a Triage type. The built-in "unclassified" type can never be deregistered.}
            - (read-only-safe) get name [str]  {Show one registered Triage type by exact name.}
            - (read-only-safe) list  {List every registered Triage type, including the built-in "unclassified" type.}
            - register name [str] --description [text] --label [text]  {Register (or update) a Triage type -- the categories the Arbiter classifies a Triage-opted-in cell into (RAL-318).}
    - (read-only-safe) tutor  {Print the Task TOML schema reference and worked examples. (Rust port hoists Python's `task show-tutor` to this top-level command.)}
    - (read-only-safe) validate file [path...]  {Validate one or more task TOML files.}
```
<!-- END GENERATED HELP-MAP (RAL-110) -->
