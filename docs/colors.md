# Ralphus UI Color Guide

The single source of truth for **what color means what** in the Ralphus web board
(`librarian/assets/board.html`). When you add or change any UI element, pick a
color from this document — do not invent an ad-hoc color or hardcode a hex value.

## Where colors are defined

All colors are CSS custom properties declared once in `board.html`:

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
| `--waiting` | `#f778ba` | *(shared)* | status (a `pending` run held back by a scheduler down-time window, RAL-122) |
| `--solo` | `#ffa657` | *(shared)* | semantic (a task marked "soloed" — its siblings are paused, RAL-157) |
| `--stale` | `#db6d28` | *(shared)* | semantic (Live View: no fresh pane output for a while from a still-running session, RAL-170) |
| `--empty` | `#ff9492` | *(shared)* | semantic (a review branch that contributes no changes — fails the review, RAL-190) |

"*(shared)*" = not overridden in the light theme; the same hue is used in both.

## Semantic roles — pick by intent

### Entity status (runs, tasks, sessions, verifies)
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
Anything the **user explicitly selected** (a selected queue row, task, run, or
session). Style: `background: rgba(74,163,255,.16); border-color: var(--accent)`
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
failure whose fix is "go look at the task's session", not "go look at the
diff". Deliberately *not* `--ignored` amber either, which is reserved for the
real `ignored` status.

Use it only for "this branch contains nothing," not as a general warning or
general failure color.

### Log severity — Cartographer only (RAL-98)
Cartographer's event table (the global log, a run's Logs "events" tab, and a
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
A run that is `pending` purely because a configured scheduler down-time
window (`[daemon]` in `.ralphus.toml`) is currently active is shown with the
label "waiting" instead of "pending", colored with `--waiting`. This is a
**display-only** relabeling of the `pending` state (driven by the board's
`GET /api/tasks` `daemon.downtime_active` flag) — the run's real stored state
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

### Read-only field indicator — `--muted` (no new color)
A detail-pane field that is purely derived/computed and can never be edited
(e.g. a session's git `upstream`, resolved live from git state rather than
stored config) marks its key label with a small `🔒` badge (`.ro-badge`),
colored `--muted` — the same "secondary/disabled text" role already used for
`--muted` elsewhere, not a new hue. Pair it with a tooltip explaining why the
field is read-only (per the UI Tooltip Rule in `CLAUDE.md`). Do **not** use
`--ignored` (amber) or `--danger`/`--failed` (red) for this — a read-only
field isn't a caution or a destructive action, just information with nowhere
to write back to.

### Stale liveness — `--stale` only (RAL-170)
The Live View peek box shows the timestamp of the last fresh pane output
received for a running session; once that gap passes a threshold
(`PEEK_STALE_WARNING_MS` in `board.html`) the timestamp switches from
`--muted` to `--stale`, a caution that a still-`running` session may have
silently hung rather than a normal quiet stretch. Distinct from the
caution-reserved `--ignored` (that amber is for the genuine `ignored`
status, not generic "pay attention") and from `--warn` (reserved for
Cartographer log severity) — `--stale` exists only because no existing role
fit this new concept (see "Adding a new UI element" below).

### Inherited resolved value — italic text only (no new color)
A detail-pane field whose displayed value is a resolved fallback from a parent
scope (for example, a session `agent` inherited from its task, or a verify
step `model` inherited from its parent session) should render its **value**
text in italics (`font-style: italic`) with the normal primary text color
(`--text`) — no new hue. Pair it with a tooltip naming the source scope (per
the UI Tooltip Rule in `CLAUDE.md`). Do **not** recolor inherited values to
`--muted`, `--ignored`, or any status hue — inheritance is provenance, not a
disabled state, warning, or status.

### Text & surfaces
- Primary text: `--text`. Secondary/muted/disabled text: `--muted`.
- Backgrounds, in increasing elevation: `--bg` → `--panel` → `--panel-2`.
- Borders/dividers: `--border`.

## Theme handling

- The dark theme is the default (`:root`). The light theme (`[data-theme="light"]`)
  overrides only the chrome colors (`--bg`, `--panel`, `--panel-2`, `--border`,
  `--text`, `--muted`, `--accent`).
- Status and semantic hues (`--running`, `--done`, `--failed`, `--pending`,
  `--queued`, `--cancelled`, `--ignored`, `--teal`, `--danger`) are **shared** —
  they read acceptably on both backgrounds, so they are defined once.
- When you add a new color: if it must differ between themes, add it to `:root`
  **and** to the `[data-theme="light"]` block. If a single hue works on both
  backgrounds (as the status colors do), define it once in `:root`.

## Adding a new UI element — checklist

1. Is there an existing semantic role above that fits? Use its variable.
2. If not, add a new `var(--name)` in `board.html` (`:root`, plus the light
   override if needed), add a row to the palette + a semantic role here, then use it.
3. Never hardcode a hex value or clone an existing hue's number inline — always go
   through a variable so the palette stays the one place colors are defined.
