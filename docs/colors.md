# Ralphus UI Color Guide

The single source of truth for **what color means what** in the Ralphus web board
(`librarian/assets/board.css`, the UI in `librarian/assets/board.html` + `board/*.js`). When you add or change any UI element, pick a
color from this document — do not invent an ad-hoc color or hardcode a hex value.

## Where colors are defined

All colors are CSS custom properties declared once in `librarian/assets/board.css`:

- `:root { … }` — the **dark theme** (the default).
- `[data-theme="light"] { … }` — the **light theme**, which overrides only the
  neutral/chrome colors. The semantic *status* colors are shared across themes.

**Rule:** every color in markup/CSS must be `var(--name)` (or an `rgba()` tint of
one of these hues). Never write a raw hex value in a rule or inline style. If you
need a color that has no variable yet, add a variable here first, document its
role in the table below, then use it.

## Palette

| Variable | Dark | Light | Kind |
|---|---|---|---|
| `--bg` | `#0d1117` | `#ffffff` | chrome (page background) |
| `--panel` | `#161b22` | `#f6f8fa` | chrome (raised surface) |
| `--panel-2` | `#1c2129` | `#eef1f4` | chrome (nested surface / hover) |
| `--border` | `#30363d` | `#d0d7de` | chrome (dividers, control borders) |
| `--text` | `#e6edf3` | `#1f2328` | chrome (primary text) |
| `--muted` | `#8b949e` | `#656d76` | chrome (secondary text, disabled) |
| `--accent` | `#4aa3ff` | `#0969da` | interactive (primary action, **selection**, focus, active tab) |
| `--danger` | `#f85149` | *(shared)* | destructive action |
| `--running` | `#4aa3ff` | *(shared)* | status |
| `--done` | `#3fb950` | *(shared)* | status |
| `--failed` | `#f85149` | *(shared)* | status |
| `--pending` | `#8b949e` | *(shared)* | status |
| `--queued` | `#a371f7` | *(shared)* | status |
| `--cancelled` | `#6e7681` | *(shared)* | status |
| `--ignored` | `#c9a227` | *(shared)* | status (amber — the **only** caution color) |
| `--teal` | `#39c5cf` | *(shared)* | semantic (dependency / linked movement) |
| `--warn` | `#d29922` | *(shared)* | semantic (Cartographer `warning`-level log severity, RAL-98) |
| `--unverified` | `#e3b341` | *(shared)* | semantic (review reached done with no build/test verification, RAL-101) |
| `--waiting` | `#f778ba` | *(shared)* | status (a `pending` squad held back by a scheduler down-time window, RAL-122) |
| `--solo` | `#ffa657` | *(shared)* | semantic (a task marked "soloed" — its siblings are paused, RAL-157) |
| `--stale` | `#db6d28` | *(shared)* | semantic (Live View: no fresh pane output for a while from a still-running cell, RAL-170) |
| `--empty` | `#ff9492` | *(shared)* | semantic (a review branch that contributes no changes — fails the review, RAL-190) |
| `--drift` | `#f0883e` | *(shared)* | semantic (a submitted PR's remote branch and its review worktree have diverged, RAL-190) |
| `--incomplete` | `#db6d28` | *(shared)* | semantic (uber-log-viewer data that may be pruned/truncated, RAL-155) |
| `--out-of-date` | `#d4a72c` | *(shared)* | semantic (a task/cell/proof step's env overrides changed since it last ran, RAL-271) |
| `--detached` | `#d2a8ff` | *(shared)* | semantic (a cell cleanly stopped mid-task for a real interactive agent session to take over, not Done/Failed/Cancelled, RAL-288) |
| `--arbiter` | `#7c3aed` | *(shared)* | semantic (a review automatically created by the Arbiter/Triage subsystem rather than an authored `[[review]]`, RAL-318) |
| `--terminal-bg` | `#000000` | *(shared)* | surface (the remote terminal relay's xterm.js panel background, RAL-355 Phase 10) |

"*(shared)*" = not overridden in the light theme; the same hue is used in both.

## Semantic roles — pick by intent

### Entity status (squads, tasks, cells, proofs)
Use the matching status variable for the status dot, badge, and any state-colored
border. The stable state string maps 1:1 to a variable of the same name:

| State | Variable |
|---|---|
| `running` | `--running` (blue) |
| `done` | `--done` (green) |
| `failed` | `--failed` (red) |
| `pending` | `--pending` (grey) |
| `queued` | `--queued` (purple) |
| `cancelled` | `--cancelled` (grey) |
| `ignored` | `--ignored` (amber) |

### Selection — `--accent` (pale blue)
Anything the **user explicitly selected** (a selected queue row, task, squad, or
cell). Style: `background: rgba(74,163,255,.16); border-color: var(--accent)`
with a **solid** border. This is the primary "you picked this" affordance and must
stay visually calm and unambiguous. Do not reuse it for anything the user did not
directly select.

### Linked / dependency-driven movement — `--teal`, **dashed** border
An item the system moved **as a side effect** of the user's action — e.g. a
dependency pulled along when its dependent was dragged above it (the Queue's
anchored drag-along). Style: `background: rgba(57,197,207,.15); border-color:
var(--teal); border-style: dashed`. Two cues separate it from a real selection:
the **teal** hue (vs. accent blue) and the **dashed** border (vs. solid) — together
they say "moved automatically, you didn't pick this." This is the same hue as the
dependency indicator labels (`↳ needs …` / `↳ after …`), tying the two ideas
together. **Never** use `--ignored` (amber) or `--failed`/`--danger` (red) for this —
they read as warnings, and an automatic, harmless move is not a warning.

### Interactive / accent — `--accent`
Primary buttons, the active tab, links, focus rings, and hover emphasis on
actionable controls.

### Destructive — `--danger` / `--failed` (red)
Delete, clear, and other irreversible actions. Pair with a confirm dialog and the
"This cannot be undone." tooltip phrasing (see the UI Tooltip Rule in CLAUDE.md).

### Caution — `--ignored` (amber)
Amber is the project's **only** caution color and is reserved for the genuine
`ignored` status. Do **not** reach for amber as generic "attention" or feedback —
if something needs a neutral highlight, use `--accent` (selection) or `--teal`
(linked), not amber.

### Empty contribution — `--empty` (RAL-190)
A review branch that rebased cleanly but adds **no diff** over the branch
beneath it — almost always because its task never committed. This *fails* the
review (`note_if_branch_is_empty` → `fail_branch`): a review must never approve
a stack containing a branch whose work it does not actually carry.

The branch's status is therefore `failed`, but it deliberately does **not** use
`--failed` red. A generic red "failed" says only that something went wrong; the
salmon `--empty` says *which* thing, distinguishing "this branch is empty" from
a conflict, a check-gate failure, or a rebase error at a glance — the one
failure whose fix is "go look at the task's cell", not "go look at the
diff". Deliberately *not* `--ignored` amber either, which is reserved for the
real `ignored` status.

Use it only for "this branch contains nothing," not as a general warning or
general failure color.

### PR/worktree drift — `--drift` only (RAL-190)
The Reviews panel's PR section uses `--drift` for exactly one case: a
submitted PR's remote branch and its owning review worktree have diverged —
most often a reviewer pushed a fix directly to the open PR branch instead of
leaving a comment. Distinct from `--ignored` (reserved for the real `ignored`
status) and from `--warn` (reserved for Cartographer log severity); `--drift`
exists only because no existing role fit this new concept. It is informational
rather than an error — the fix is one click ("Pull PR commits"), not a
failure requiring investigation, so it deliberately does not reuse
`--failed`/`--danger` red either.

### PR CI/CD status — `--done`/`--failed`/`--pending` (RAL-395)
The Reviews panel's PR card/link (`librarian/assets/board/65-reviews.js`) and
the Tasks tab's PR chip (`librarian/assets/board/15-tasks.js`,
`10-tab-registry.js`) both color an **open** PR by its last-polled CI/CD
status instead of the flat "in-flight" `--accent` a PR's lifecycle state
alone would otherwise get: `--done` (green) for `passing`, `--failed` (red)
for `failing`, `--pending` (grey) for a status not yet known. This reuses the
same three status roles already documented above for `done`/`failed`/
`pending` entity states — CI/CD pass/fail/pending is the same concept
("did the work verify"), just scoped to a PR's remote checks instead of a
cell/task/proof — so no new color was added. Once a PR is no longer `open`
(merged/closed/dropped), its CI status is moot and the badge reverts to the
existing PR-lifecycle coloring (`--accent`/`--done`/`--cancelled`/`--failed`)
documented for `TT_PR_COLORS`/`PR_STATE_COLORS`.

### Log severity — Cartographer only (RAL-98)
Cartographer's event table (the global log, a squad's Logs "events" tab, and a
review's Logs button) colors rows by `level`, a concept distinct from entity
status: `error` reuses `--failed` (red), `warning` uses the dedicated `--warn`
(a separate hue from `--ignored` — log severity is not the same concept as the
caution-reserved `ignored` status), and `info`/`debug`/`trace` use the neutral
`--pending`/`--cancelled` grays. Do not reuse `--warn` outside log-level
display; it exists only because no existing role fit this new concept
(see "Adding a new UI element" below).

### Unverified review — `--unverified` only (RAL-101)
The Reviews panel's "check gates" section uses `--unverified` for exactly one
case: a review reached `in_review` with **no** explicit check gates *and* no
project `auto_build` default configured, so it carries zero build/test
verification. This is a distinct concept from the caution-reserved `ignored`
status and from Cartographer's `--warn` log severity — reuse neither for it;
`--unverified` exists only because no existing role fit this new concept
(see "Adding a new UI element" below).

### Down-time waiting — `--waiting` only (RAL-122)
A squad that is `pending` purely because a configured scheduler down-time
window (`[daemon]` in `.ralphus.toml`) is currently active is shown with the
label "waiting" instead of "pending", colored with `--waiting`. This is a
**display-only** relabeling of the `pending` state (driven by the board's
`GET /api/tasks` `daemon.downtime_active` flag) — the squad's real stored state
is still `pending`, so status filters, menus, and the API are unaffected.
Distinct from `--pending` (grey, "the scheduler hasn't gotten to this yet")
and from the caution-reserved `--ignored` (this is expected, configured
behavior, not a warning) — `--waiting` exists only because no existing role
fit this new concept (see "Adding a new UI element" below).

### Soloed task — `--solo` only (RAL-157)
A task marked "soloed" (right-click → Solo task) shows a small `★ solo` badge
next to its status pill, colored `--solo`. `soloed` is an independent boolean
flag orthogonal to the task's own `state` (a soloed task can be pending,
running, or done), so it can't reuse a status color; it also isn't a user
*selection* (`--accent`), a dependency-driven move (`--teal`), or a caution
(`--ignored`) — `--solo` exists only because no existing role fit this new
concept (see "Adding a new UI element" below).

### Possibly-incomplete data — `--incomplete` only (RAL-155)
The uber-log-viewer (a squad's "Timeline" button/modal) flags two best-effort
conditions with `--incomplete`: `gaps_possible` (Cartographer's retention
pruning has already removed some of this squad's earliest history) and
`truncated` (the squad generated more events than the conservative
per-generation cap). Both are "this data may not be the full picture," not a
caution about an action the user is about to take (`--ignored`), a log
severity (`--warn`, Cartographer-display-only), or an unverified-review state
(`--unverified`) — `--incomplete` exists only because no existing role fit
this new concept (see "Adding a new UI element" below).

### Read-only field indicator — `--muted` (no new color)
A detail-pane field that is purely derived/computed and can never be edited
(e.g. a cell's git `upstream`, resolved live from git state rather than
stored config) marks its key label with a small `🔒` badge (`.ro-badge`),
colored `--muted` — the same "secondary/disabled text" role already used for
`--muted` elsewhere, not a new hue. Pair it with a tooltip explaining why the
field is read-only (per the UI Tooltip Rule in `CLAUDE.md`). Do **not** use
`--ignored` (amber) or `--danger`/`--failed` (red) for this — a read-only
field isn't a caution or a destructive action, just information with nowhere
to write back to.

### Stale liveness — `--stale` only (RAL-170)
The Live View peek box shows the timestamp of the last fresh pane output
received for a running cell; once that gap passes a threshold
(`PEEK_STALE_WARNING_MS` in `librarian/assets/board/35-terminal-logs.js`) the timestamp switches from
`--muted` to `--stale`, a caution that a still-`running` cell may have
silently hung rather than a normal quiet stretch. Distinct from the
caution-reserved `--ignored` (that amber is for the genuine `ignored`
status, not generic "pay attention") and from `--warn` (reserved for
Cartographer log severity) — `--stale` exists only because no existing role
fit this new concept (see "Adding a new UI element" below).

### Env overrides "out of date" — `--out-of-date` only (RAL-271)
A task/cell/proof step's Details Pane shows a small `⚠ env out of date` badge
next to its status pill once its own environment-variable overrides have
been edited since it last ran/retried or had its status explicitly set
(`Set Status`). This is purely cosmetic — no behavior changes and no proof
result is invalidated — so it deliberately does not reuse `--failed`/
`--danger` (nothing failed), `--ignored` (reserved for the real `ignored`
status), or `--drift` (reserved for PR/worktree divergence, RAL-190,
a different concept even though both are "stored state diverged from
current"); `--out-of-date` exists only because no existing role fit this
new concept (see "Adding a new UI element" below).

### Detached cell — `--detached` only (RAL-288)
A task cell's status pill grows a `⏸ detached` badge while it reads `running`
but has actually been cleanly stopped for a human's real interactive agent
session to take over (the "Open Agent" action). The cell is not stuck or
stalled — its Live View pane legitimately goes quiet and shows the historical
record of the now-ended headless process, since the live conversation moved
to a separate tmux session outside the daemon's own polling — but the cell
itself has not failed, finished, or been cancelled either, so it deliberately
does not reuse `--stale` (reserved for a *still headlessly running* cell that
has gone quiet, a different and more alarming situation), `--ignored`
(reserved for the real `ignored` status), or `--muted` (would read as "just a
normal read-only field", losing the signal entirely); `--detached` exists
only because no existing role fit this new concept (see "Adding a new UI
element" below). Clears automatically the next time the cell is dispatched
(a restart, or the explicit resume-automation trigger).

### Arbiter-created review — `--arbiter` only (RAL-318)
The Reviews panel marks a review with a small "⚙ Arbiter" badge when its
`origin` is `"arbiter"` — created automatically by the Arbiter/Triage
subsystem when a pooled cell count threshold or cron schedule fired, rather
than from an authored `[[review]]` block or any other pre-existing creation
path (the `explicit` origin, true of essentially every review that exists
today). This is provenance — "the Arbiter made this, not a human/task file" —
not a status (`--running`/`--done`/etc. already cover review state), a
caution (`--ignored` is reserved for the real `ignored` status), or a
selection/linked-movement cue (`--accent`/`--teal`); `--arbiter` exists only
because no existing role fit this new concept (see "Adding a new UI element"
below).

The same role, and the exact same badge markup, also marks a cell whose
monorepo **subproject** (RAL-346, see `docs/glossary.md`) was inferred by the
Arbiter's async description-matching step rather than seeded from the cell's
own manually-declared `subprojects` TOML field — it's the identical
provenance signal ("the Arbiter made this, not a human"), just attached to a
different field, so it reuses `--arbiter` rather than adding a second
purple-ish variable for what is semantically the same concept.

### Inherited resolved value — italic text only (no new color)
A detail-pane field whose displayed value is a resolved fallback from a parent
scope (for example, a cell `agent` inherited from its task, or a proof
step `model` inherited from its parent cell) should render its **value**
text in italics (`font-style: italic`) with the normal primary text color
(`--text`) — no new hue. Pair it with a tooltip naming the source scope (per
the UI Tooltip Rule in `CLAUDE.md`). Do **not** recolor inherited values to
`--muted`, `--ignored`, or any status hue — inheritance is provenance, not a
disabled state, warning, or status.

### Terminal surface — `--terminal-bg` only (RAL-355 Phase 10)
The remote Open Agent terminal relay's xterm.js panel always renders on a
fixed near-black background (`--terminal-bg`), not `--bg`/`--panel` — a
terminal pane conventionally stays dark regardless of the surrounding
theme, the same way a real terminal emulator's background doesn't follow
the host OS's light/dark setting, and xterm.js's own ANSI color rendering
assumes a dark backdrop. Shared across both themes (not overridden in
`[data-theme="light"]`) for that reason. Set via xterm.js's own `theme`
option (a JS value, not CSS) by reading the CSS variable's resolved value
at runtime, so there is still exactly one place this color is defined.

### Text & surfaces
- Primary text: `--text`. Secondary/muted/disabled text: `--muted`.
- Backgrounds, in increasing elevation: `--bg` → `--panel` → `--panel-2`.
- Borders/dividers: `--border`.

## Theme handling

- The dark theme is the default (`:root`). The light theme (`[data-theme="light"]`)
  overrides only the chrome colors (`--bg`, `--panel`, `--panel-2`, `--border`,
  `--text`, `--muted`, `--accent`).
- Status and semantic hues (`--running`, `--done`, `--failed`, `--pending`,
  `--queued`, `--cancelled`, `--ignored`, `--teal`, `--danger`, `--incomplete`) are **shared** —
  they read acceptably on both backgrounds, so they are defined once.
- When you add a new color: if it must differ between themes, add it to `:root`
  **and** to the `[data-theme="light"]` block. If a single hue works on both
  backgrounds (as the status colors do), define it once in `:root`.

## Adding a new UI element — checklist

1. Is there an existing semantic role above that fits? Use its variable.
2. If not, add a new `var(--name)` in `board.css` (`:root`, plus the light
   override if needed), add a row to the palette + a semantic role here, then use it.
3. Never hardcode a hex value or clone an existing hue's number inline — always go
   through a variable so the palette stays the one place colors are defined.
