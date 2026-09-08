# librarian/

`ralphus-librarian` serves the web board: static assets and a proxy of
`/api/*` GETs to the daemon. The UI lives in `librarian/assets/` as plain
HTML + vanilla JS with no build step and no TypeScript source files:

```
librarian/assets/
  board.html            # page shell: head, body markup, <script src> tags — no inline JS
  board.css             # the whole stylesheet (dark + light theme variables)
  board/*.js            # the board's JavaScript, one file per section, loaded in
                        # filename order via board.html's sequential <script> tags
  vendor/               # vendored xterm.js + xterm-addon-fit (RAL-355), UMD globals
```

The chunk files are plain global-scope scripts — no `import`/`export` — so
every top-level declaration is visible to every other chunk at runtime.

Serving has two modes (see `src/assets.rs`):
- **Release:** `build.rs` bakes every board asset into the exe as an
  embedded `(route, contents)` table (`BOARD_ASSETS`), so the binary is
  self-contained.
- **Dev:** `scripts/build-debug.sh` exports `RALPHUS_BOARD_ASSETS_DIR`, and
  the librarian then reads board assets from the checkout on every request —
  **an edit to a chunk/CSS/shell file is one browser-refresh away, no
  rebuild**. Missing files 404 rather than fall back to stale embedded
  copies.

Route names are validated by the shared rules in `src/asset_rules.rs`:
the shell and stylesheet, every `*.js` under `board/`, and exactly the three
vendored xterm files. Nothing else is ever served or embedded — both the
build-time walk and the dev-mode disk reader include the same rules file, and
Rust tests in `src/assets.rs` pin them against the on-disk truth.

```bash
npm install --no-audit --no-fund
npm run lint                        # eslint (chunks), incl. JSDoc-coverage rules
npm run typecheck                   # tsc --checkJs over the JSDoc-annotated chunk JS
npm run knip                        # dead code / unused deps
npm test                            # node --test over the board's pure, DOM-free logic
```

## Web JSDoc type hints (board/*.js)

Type safety comes from JSDoc annotations checked by `tsc --checkJs`
(TypeScript-the-tool, used purely as a JSDoc type-checker — no `.ts` file is
ever compiled or shipped).

**Rule: any change to the board's JavaScript (`librarian/assets/board/*.js`) —
a new function, an added/changed parameter, a new shared shape used across
functions — must ship with matching JSDoc type hints (`@param`/`@returns`,
and a `@typedef` for any new shared object shape). Before considering the
change done, run:**

```bash
npm install --no-audit --no-fund   # first time / after package.json changes
npm run lint                       # eslint, incl. jsdoc/require-* coverage rules
npm run typecheck                  # tsc --checkJs; errors point at the chunk file directly
```

**Both must pass clean.** This applies to human and AI-driven changes alike —
neither the Rust tests nor `cargo`/`uv` checks catch a JS type regression
here; the `web` job in `.github/workflows/ci.yml` is the only thing that
does, and it runs exactly these two commands (plus `npm run knip` and
`npm test` — see "Frontend — board chunks" below).

- Every top-level `function name(...) {}` declaration has a JSDoc comment
  with `@param`/`@returns` tags; `jsdoc/require-jsdoc` (in `eslint.config.mjs`)
  enforces this for new code. Small inline arrow-function callbacks (e.g.
  `.map((x) => ...)`) are intentionally exempt — annotating every one would be
  noise, not signal.
- Shared wire shapes (`SquadView`, `TaskView`, `CellView`, `GuardianView`,
  `QueueItem`, `CartographerRow`, ...) are declared once as `@typedef` blocks
  in `board/00-typedefs.js` and referenced by name from function signatures —
  see `docs/daemon-api.md` for the shapes' authoritative source.
- `npm run typecheck` runs `tsc -p tsconfig.board.json` directly over the
  chunk files. Because they are plain scripts (not modules), tsc treats their
  top-level declarations as one shared global scope — exactly what the browser
  sees — so a name declared in one chunk resolves in all others, with no
  extraction step and no path rewriting. `tsconfig.board.json` sets
  `"strict": true`, so this is full TypeScript strict mode: `noImplicitAny`
  (a missing annotation is a hard error) *and* `strictNullChecks` (a
  `T|null`/`T|undefined` value must be narrowed — e.g. checked, defaulted with
  `??`/`||`, or captured into a local `const` right after a truthy check —
  before it's used as a plain `T`). ESLint analyzes each chunk in isolation,
  so cross-chunk names come from the generated globals map
  (`.lint-tmp/board-globals.json`, built by
  `scripts/generate-board-globals.mjs` as the first half of `npm run lint`);
  a reference to a name no chunk declares still fails `no-undef`.
- **Null-narrowing idioms used throughout the board chunks:**
  - `document.getElementById(id)` for an element that's always present in the
    page's static structure uses the `byId(id)` helper (defined in
    `board/20-util.js`, alongside `esc`/`cvar`), which asserts non-null via a
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
  single-line `/** ... */` block in the chunk files that needs it — reach for
  it again if a bulk edit reintroduces single-line multi-tag comments. Bare
  `@type`/`@typedef`-only comments with no `@param`/`@returns` are fine to
  leave single-line.

## Knip (dead code / unused deps) for the board chunks

The chunks are plain global-scope scripts — no `import`/`export` — so knip's
cross-file reachability graph has nothing to walk inside them.
`knip.config.js` (repo root) registers the board.html page shell as the
entry with a custom `html` **compiler** that inlines the chunk files in
place of the shell's `<script src>` tags, faking one module over the whole
board so knip catches **intra-chunk dead functions**:

every top-level `function`/`async function`/`const ... = (...) =>` gets
rewritten to carry `export` (`includeEntryExports: true` is required
alongside this, since entry-file exports are normally presumed to be public
API and skipped), and every other occurrence of `declaredName(` anywhere in
the assembled chunks or the shell's own markup — including inside
`onclick="..."` strings and multi-layer indirection like
`terminalMenuItem(key, label, \`foo(...)\`, tip)`, neither of which any real
parser (ESLint included — the same reason `no-unused-vars` stays disabled in
`eslint.config.mjs`) can see into — is collected into a synthetic reference
sink appended to the compiled output, so those string-only-dispatched
handlers don't false-positive as unused (`ignoreExportsUsedInFile: true` is
also required, since by default knip only counts cross-file imports as
"used", not same-file references — which is exactly what the sink produces).
A name with zero occurrences anywhere in the chunks or the shell is
genuinely dead. See the comment block at the top of `knip.config.js` for the
full mechanics and known limitations (it's a heuristic, not real reference
tracking — safe to miss a dead function, e.g. one dispatched via
`window[name]()`, but should not false-positive on any string-wiring pattern
actually used in this codebase).

`npm run knip` is `scripts/knip-board.mjs`: it recomputes the compiler's
same inlining layout and rewrites knip's diagnostics to the real per-chunk
paths, so an error always names the chunk file (and line) to edit. The
chunk and vendor directories are in knip's `ignore` list (they are inlined
into the compiled entry, not imported); the shell itself must NOT be
ignored. Knip otherwise audits every other JS file in this Node package
(`eslint.config.mjs`, `scripts/*.mjs`) via its default project glob.

## Testing — Frontend (board chunks)

The board is plain HTML + vanilla JS served as static files, so most of it
is only testable in a browser. The exception (RAL-186) is its **pure,
DOM-free logic**, which `npm test` (`node --test`, files in `test/`)
exercises directly:

```bash
npm test                            # from repo root; also a CI step in the `web` job
```

`test/board-source.mjs` concatenates the chunk files in load order (sorted
filename order — the same order `board.html`'s `<script>` tags and knip's
compiler use). Because the chunks are a byte-exact split of the board's
script body, that text is exactly what the browser executes. The loader
files in `test/` (e.g. `test/board-peek-state.mjs`) slice marker regions —
`// RALPHUS-PEEK-STATE-MACHINE:BEGIN` / `:END` — out of that concatenation
and evaluate them standalone. **The tests read the real shipped source;
there is no second copy to drift from.** If a region moves, move the markers
with it, or the loader fails loudly. To extend this pattern to another piece
of board logic, factor the decision-making part free of DOM/`fetch`/
module-level state (the way `nextPeekPaneState` is: state in, state out) and
give it its own marker region — that separation is what makes it testable at
all.

`librarian/tests/board-long-running.test.mjs` extracts whole functions by
name from the same concatenation (asserting that the daemon's ported
board-logic twins still match the UI's implementation).

Everything else about the board is still tested manually:

1. Run `bash scripts/build-debug.sh` (boots daemon + librarian; see [[../scripts/AGENTS|scripts/AGENTS.md]]).
2. Open `http://127.0.0.1:7474` in a browser.
3. Submit a task and watch the board update (the daemon pushes changes over SSE;
   open Live View boxes refresh on their own 2 s interval).
4. Exercise the tab you changed (Squads, Reviews, etc.).

Frontend dev loop: edit `librarian/assets/board/*.js` (or `board.css` / the
shell) → refresh the browser. Dev mode reads the assets from disk
(`RALPHUS_BOARD_ASSETS_DIR`, set by `scripts/build-debug.sh`), so there is
no rebuild at all.

## UI Tooltip Rule (RAL-40)

Every new UI element in the board **must ship with a tooltip** using the
`data-tip="..."` attribute. The tooltip engine is a lightweight JS+CSS system
already wired into the page (see the `// ---- Tooltip engine (RAL-40) ----`
block in `librarian/assets/board/05-engines.js` and the `#board-tip` CSS
rule in `board.css`). See also "Web JSDoc type hints (board/*.js)" above —
the other mandatory checklist item (`npm run lint && npm run typecheck`) for
any board change.

**Implementation pattern:** add `data-tip="..."` to any HTML element — static
(in `board.html`) or dynamically generated in a JS template string (in a
chunk). The engine uses event delegation on `mouseover`/`mouseout` and
renders a fixed-position dark-themed popover near the cursor. Multi-line
content: use `\n` in the attribute value.

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

**Any time you choose a color for the web board or any UI, consult
[`docs/colors.md`](../docs/colors.md) and use the documented CSS variable or
rule.** That document is the single source of truth for what each color
means (status colors, `--accent` for selection, `--teal` for
dependency-driven "pulled along" movement, `--ignored` amber reserved for
caution, etc.).

Rules:
- Never hardcode a hex value or invent an ad-hoc color. Always use `var(--name)`
  (or an `rgba()` tint of a documented hue).
- If no existing semantic role fits, add a new variable in `librarian/assets/board.css`
  (and its light-theme override if needed), document it in `docs/colors.md`,
  *then* use it.
- Every new UI element also needs a `data-tip` tooltip — see "UI Tooltip Rule
  (RAL-40)" above.
