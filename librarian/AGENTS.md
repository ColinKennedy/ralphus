# librarian/

`ralphus-librarian` serves the web board: static HTML and a proxy of
`/api/*` GETs to the daemon. The entire dark-theme UI is plain HTML + inline
JS in `librarian/assets/board.html`, embedded via `include_str!` — no build
step, no TypeScript source files.

```bash
npm install --no-audit --no-fund
npm run lint                        # eslint, incl. JSDoc-coverage rules
npm run typecheck                   # tsc --checkJs over the JSDoc-annotated JS
npm run knip                        # dead code / unused deps
npm test                            # node --test over board.html's pure, DOM-free logic
```

## Web JSDoc type hints (board.html)

Type safety comes from JSDoc annotations checked by `tsc --checkJs`
(TypeScript-the-tool, used purely as a JSDoc type-checker — no `.ts` file is
ever compiled or shipped).

**Rule: any change to `librarian/assets/board.html`'s inline JavaScript —
a new function, an added/changed parameter, a new shared shape used across
functions — must ship with matching JSDoc type hints (`@param`/`@returns`,
and a `@typedef` for any new shared object shape). Before considering the
change done, run:**

```bash
npm install --no-audit --no-fund   # first time / after package.json changes
npm run lint                       # eslint, incl. jsdoc/require-* coverage rules
npm run typecheck                  # tsc --checkJs; errors point at board.html directly
```

**Both must pass clean.** This applies to human and AI-driven changes alike —
neither the Rust "does the HTML contain X" tests nor `cargo`/`uv` checks catch
a JS type regression here; the `web` job in `.github/workflows/ci.yml` is the
only thing that does, and it runs exactly these two commands (plus `npm run
knip` and `npm test` — see "Frontend — board.html" below).

- Every top-level `function name(...) {}` declaration has a JSDoc comment
  with `@param`/`@returns` tags; `jsdoc/require-jsdoc` (in `eslint.config.mjs`)
  enforces this for new code. Small inline arrow-function callbacks (e.g.
  `.map((x) => ...)`) are intentionally exempt — annotating every one would be
  noise, not signal.
- Shared wire shapes (`SquadView`, `TaskView`, `CellView`, `GuardianView`,
  `QueueItem`, `CartographerRow`, ...) are declared once as `@typedef` blocks
  near the top of the `<script>` and referenced by name from function
  signatures — see `docs/daemon-api.md` for the shapes' authoritative source.
- `npm run typecheck` extracts the `<script>` body to `.lint-tmp/board.js`
  (via `scripts/extract-board-js.mjs`, preserving line numbers) and runs
  `tsc -p tsconfig.board.json` via `scripts/typecheck-board.mjs` —
  `tsconfig.board.json` sets `"strict": true`, so this is full TypeScript
  strict mode: `noImplicitAny` (a missing annotation is a hard error) *and*
  `strictNullChecks` (a `T|null`/`T|undefined` value must be narrowed —
  e.g. checked, defaulted with `??`/`||`, or captured into a local `const`
  right after a truthy check — before it's used as a plain `T`). Every
  `.lint-tmp/board.js` path in `tsc`'s diagnostic output is rewritten to
  `librarian/assets/board.html` (line/column numbers already line up 1:1, so
  this is a pure text substitution) — both locally and in CI, an error always
  points at the real file to edit, never the disposable extracted one.
- **Null-narrowing idioms used throughout board.html:**
  - `document.getElementById(id)` for an element that's always present in the
    page's static structure uses the `byId(id)` helper (defined near the top
    of the `<script>`, alongside `esc`/`cvar`), which asserts non-null via a
    `/** @type {HTMLElement} */` cast — same behavior as the original
    unguarded call (still throws if the id is ever missing), just satisfies
    the checker. For an element that's only conditionally rendered, keep the
    existing `const el = document.getElementById(id); if (el) ...` guard
    pattern instead.
  - A value narrowed by a truthy check on one line (`if (x.foo) ...`) does
    **not** stay narrowed inside a nested callback, nor for an *unrelated*
    variable that merely aliases it (e.g. `const y = x; y.foo` after checking
    `x.foo`), nor for a captured outer `let`/mutable-global after any
    intervening function call. Capture the narrowed value into its own local
    `const` right where you need it, rather than re-deriving it inside a
    closure.
  - `Array.prototype.filter(Boolean)` does not narrow `(T|undefined)[]` to
    `T[]` for TypeScript — follow it with `/** @type {T[]} */ (...)`.
- **Gotcha:** `eslint-plugin-jsdoc`'s comment parser does not reliably
  recognize an `@param`/`@returns` tag that shares a physical line with the
  description or with another tag — cram `/** Does X. @param {string} a
  @returns {void} */` onto one line and `jsdoc/require-param`/
  `require-returns` report *incorrect* "missing" errors for the tags after
  the first. Any JSDoc comment carrying a `@param`/`@returns` tag must be
  standard multi-line (one tag per line). `scripts/reformat-jsdoc.mjs` is a
  one-off (but rerunnable) tool that mechanically reformats every
  single-line `/** ... */` block in board.html that needs it — reach for it
  again if a bulk edit reintroduces single-line multi-tag comments. Bare
  `@type`/`@typedef`-only comments with no `@param`/`@returns` are fine to
  leave single-line.

## Knip (dead code / unused deps) for board.html

`knip.config.js` (repo root) registers a custom knip **compiler** for the
`html` extension that lets knip analyze `librarian/assets/board.html`
directly — no separate extracted file and no path-rewritten output, unlike
the ESLint/tsc setup above; knip's diagnostics already point at the real
file. `board.html`, `knip.config.js` itself, and
`scripts/reformat-jsdoc.mjs` (a manually-invoked tool, never imported or run
from an npm script) are listed under `entry` so knip doesn't flag them as
unreachable. `project` is left at knip's default glob
(`**/*.{js,mjs,cjs,jsx,ts,tsx,mts,cts}`, gitignore-filtered), so `npm run
knip` also audits every other JS file in this Node package
(`eslint.config.mjs`, `scripts/*.mjs`), not just board.html.

board.html's script is a plain global script (not an ES module — no
`import`/`export`), so out of the box knip's cross-file reachability graph
has nothing to walk inside it — it could only tell whether board.html itself
is reachable, not whether any function *inside* it is dead. The compiler
(`compileHtml()` in `knip.config.js`) goes further, faking a module graph
over the single file so knip also catches **intra-file dead functions**:
every top-level `function`/`async function`/`const ... = (...) =>` gets
rewritten to carry `export` (`includeEntryExports: true` is required
alongside this, since entry-file exports are normally presumed to be public
API and skipped), and every other occurrence of `declaredName(` anywhere in
the raw document — including inside `onclick="..."` strings and multi-layer
indirection like `terminalMenuItem(key, label, \`foo(...)\`, tip)`, neither
of which any real parser (ESLint included — the same reason
`no-unused-vars` stays disabled in `eslint.config.mjs`) can see into — is
collected into a synthetic reference sink appended to the compiled output,
so those string-only-dispatched handlers don't false-positive as unused
(`ignoreExportsUsedInFile: true` is also required, since by default knip
only counts cross-file imports as "used", not same-file references — which
is exactly what the sink produces). A name with zero occurrences anywhere
else in the document is genuinely dead. See the comment block at the top of
`knip.config.js` for the full mechanics and known limitations (it's a
heuristic, not real reference tracking — safe to miss a dead function, e.g.
one dispatched via `window[name]()`, but should not false-positive on any
string-wiring pattern actually used in this codebase). Run it with
`npm run knip`.

## Testing — Frontend (board.html)

The board is plain HTML + vanilla JS embedded via `include_str!`, so most of it
is only testable in a browser. The exception (RAL-186) is its **pure, DOM-free
logic**, which `npm test` (`node --test`, files in `test/`) exercises directly:

```bash
npm test                            # from repo root; also a CI step in the `web` job
```

`test/board-peek-state.mjs` slices the live-view (peek) state machine straight
out of `board.html` — the region between the `// RALPHUS-PEEK-STATE-MACHINE:BEGIN`
/ `:END` markers — and evaluates it standalone. **The tests read the real
shipped source; there is no second copy to drift from.** If that logic moves,
move the markers with it, or the loader fails loudly. To extend this pattern to
another piece of board logic, factor the decision-making part free of DOM/`fetch`/
module-level state (the way `nextPeekPaneState` is: state in, state out) and give
it its own marker region — that separation is what makes it testable at all.

Everything else about the board is still tested manually:

1. Run `bash scripts/build-debug.sh` (boots daemon + librarian; see [[../scripts/AGENTS|scripts/AGENTS.md]]).
2. Open `http://127.0.0.1:7474` in a browser.
3. Submit a task and watch the board update (the daemon pushes changes over SSE;
   open Live View boxes refresh on their own 2 s interval).
4. Exercise the tab you changed (Squads, Reviews, etc.).

Frontend dev loop: edit `librarian/assets/board.html` → re-run `bash scripts/build-debug.sh` → refresh browser. One incremental librarian recompile is the cost.

## UI Tooltip Rule (RAL-40)

Every new UI element in `librarian/assets/board.html` **must ship with a tooltip** using the `data-tip="..."` attribute. The tooltip engine is a lightweight JS+CSS system already wired into the page (see the `// ---- Tooltip engine (RAL-40) ----` block in the `<script>` tag and the `#board-tip` CSS rule). See also "Web JSDoc type hints (board.html)" above — the other mandatory checklist item (`npm run lint && npm run typecheck`) for any `board.html` change.

**Implementation pattern:** add `data-tip="..."` to any HTML element — static or dynamically generated in a JS template string. The engine uses event delegation on `mouseover`/`mouseout` and renders a fixed-position dark-themed popover near the cursor. Multi-line content: use `\n` in the attribute value.

**Required tooltip content:**
1. **Why** — the purpose of the element (what it does and why it matters).
2. **Who / when** — the scenario in which someone would use it.
3. **Caveats / warnings** — irreversible or destructive actions **must** include the phrase "This cannot be undone." even if a `confirm()` dialog also exists.

**Example:**
```html
<!-- Static HTML -->
<button data-tip="Delete this squad and all its data permanently.\nThis cannot be undone." ...>🗑 Delete</button>

<!-- JS template string -->
items.push(`<div data-tip="Cancel this squad — stops all running cells." onclick="...">■ Cancel</div>`);
```

**Do not use the native `title` attribute** for new tooltips — it renders with browser default styling and ignores the dark theme. The `title` attribute can remain on existing splitter elements (they already use `data-tip`) but should not be added to new elements.

## Design / UI colors

**Any time you choose a color for the web board (`librarian/assets/board.html`) or
any UI, consult [`docs/colors.md`](../docs/colors.md) and use the documented CSS
variable or rule.** That document is the single source of truth for what each color
means (status colors, `--accent` for selection, `--teal` for dependency-driven
"pulled along" movement, `--ignored` amber reserved for caution, etc.).

Rules:
- Never hardcode a hex value or invent an ad-hoc color. Always use `var(--name)`
  (or an `rgba()` tint of a documented hue).
- If no existing semantic role fits, add a new variable in `board.html` (and its
  light-theme override if needed), document it in `docs/colors.md`, *then* use it.
- Every new UI element also needs a `data-tip` tooltip — see "UI Tooltip Rule
  (RAL-40)" above.
