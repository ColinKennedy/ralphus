// Knip config for the librarian's board chunks (librarian/assets/board/*.js).
//
// The chunks are plain global-scope scripts — no import/export — so knip's
// cross-file reachability graph has nothing to walk inside them. The
// librarian's board.html page shell is registered as the entry (knip cannot
// parse HTML, hence the `html` compiler below), and compileHtml() fakes a
// module over the page by inlining the chunk files in place of the shell's
// <script src> tags, so knip sees them as one scope and also catches
// intra-chunk dead code:
//
//   1. Every top-level `function foo(...)` / `async function foo(...)` /
//      `const foo = (...) => ...` in the chunks is rewritten to carry
//      `export`, so it enters knip's exports analysis at all
//      (`includeEntryExports: true` is required alongside this, since
//      entry-file exports are normally presumed to be public API and
//      skipped).
//   2. Every OTHER occurrence of `declaredName(` anywhere in the assembled
//      chunks OR in the shell's own markup (i.e. excluding each function's
//      own declaration site) is collected into a synthetic `void [...]`
//      reference sink appended to the compiled output. This is the crux:
//      most of the board's handlers are wired via inline HTML on*
//      attributes in the shell or dynamically-built `onclick="...foo(...)"`
//      strings inside the chunks' JS template literals — plain text as far
//      as any parser is concerned, invisible to real reference tracking (the
//      same reason ESLint's `no-unused-vars` is disabled in
//      eslint.config.mjs). The sink turns each such name into one real,
//      trivial AST reference, so knip's reference finder counts it as used
//      without needing to understand HTML or string contents at all.
//   3. `ignoreExportsUsedInFile: true` is also required: by default knip
//      only counts an export as "used" if some *other* file imports it — a
//      same-file reference (exactly what the sink produces) is otherwise
//      still reported unused, since exporting normally signals "meant to be
//      imported elsewhere". With this flag, same-file usage counts too.
//
// Net effect: a name left with zero occurrences anywhere in the chunks or
// the shell — not in real code, not inside any on*="..." string, not passed
// through a helper like `terminalMenuItem(key, label, \`foo(...)\`, tip)` —
// is genuinely dead and gets reported. Verified against the board source:
// found 3 true positives (openVerifyNodeMenu, setVerifyMidResolution,
// queueStabilize — all superseded/orphaned handlers with zero other
// references) and zero false positives after the sink was in place,
// including for functions wired through several layers of string
// indirection.
//
// Caveats — this is a heuristic, not real reference tracking, and can in
// principle miss a genuinely dead function (a false negative, which is
// safe) if:
//   - it's dispatched via fully-dynamic lookup, e.g. `window[name]()` or a
//     dispatch table, where the literal name string never appears next to
//     an opening paren; or
//   - its name happens to coincide with some unrelated call elsewhere in
//     the assembled source (e.g. a same-named property/method on a
//     different object) — low risk given this codebase's descriptive,
//     distinctive naming.
// It will not produce a false positive from any of the string-based wiring
// patterns above, since those are exactly what the sink is built to catch.
//
// Line mapping: the compiled output is blank-padded so the shell's own
// lines keep their board.html line numbers, and the chunks follow them in
// order. `npm run knip` (scripts/knip-board.mjs) recomputes that same
// layout and rewrites diagnostics to the real per-chunk paths, so an error
// always names the chunk file to edit.

import { readFileSync, readdirSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const repoRoot = dirname(fileURLToPath(import.meta.url));
const boardDir = join(repoRoot, "librarian", "assets", "board");

// The chunks are plain global-script JS in original load order, indented the
// same way the board's script body was before the split (6 spaces at top
// level), so these regexes match the same declaration shapes the single-file
// era compiled.
const TOPLEVEL_FN_RE = /^(\s{6})(async\s+)?function\s+([A-Za-z_$][\w$]*)\s*\(/gm;
const TOPLEVEL_ARROW_RE = /^(\s{6})(const|let)\s+([A-Za-z_$][\w$]*)\s*=\s*(async\s+)?\(/gm;
const CALL_RE = /\b([A-Za-z_$][\w$]*)\s*\(/g;
const DECL_RE = /^(\s{6})(async\s+)?function\s+([A-Za-z_$][\w$]*)\s*\(/gm;
const ARROW_DECL_RE = /^(\s{6})(const|let)\s+([A-Za-z_$][\w$]*)\s*=\s*(async\s+)?\(/gm;

const SCRIPT_SRC_RE = /^.*<script src="\/board\//m;

/**
 * Inlines the board chunk files into the shell in place of its
 * `<script src="/board/...">` tags and rewrites every top-level
 * function/arrow declaration into an export, plus a synthetic reference
 * sink so string-only-dispatched handlers don't false-positive as unused
 * (see the file header for the full rationale). Knip calls compilers as
 * `(text, filePath)` — the file's contents first — so `text` is the shell.
 * @param {string} text the board.html page shell
 * @param {string} _filePath the shell's path (unused)
 * @returns {string} compiled pseudo-module, blank-padded to board.html's line numbers
 */
function compileHtml(text, _filePath) {
  const shell = text;
  const files = readdirSync(boardDir)
    .filter((f) => f.endsWith(".js") && !f.startsWith("."))
    .sort();
  const chunks = files.map((f) => readFileSync(join(boardDir, f), "utf8"));
  const contents = chunks.join("");
  const scriptStart = shell.slice(0, shell.search(SCRIPT_SRC_RE)).split("\n").length;

  const declared = new Set();
  for (const m of contents.matchAll(TOPLEVEL_FN_RE)) declared.add(m[3]);
  for (const m of contents.matchAll(TOPLEVEL_ARROW_RE)) declared.add(m[3]);

  // Blank out each declaration's own header so it can't match CALL_RE and
  // count as a reference to itself.
  const scanText = contents
    .replace(DECL_RE, (m) => " ".repeat(m.length))
    .replace(ARROW_DECL_RE, (m) => " ".repeat(m.length));

  const referenced = new Set();
  for (const source of [scanText, shell]) {
    for (const call of source.matchAll(CALL_RE)) {
      if (declared.has(call[1])) referenced.add(call[1]);
    }
  }

  const exportedScript = contents
    .replace(TOPLEVEL_FN_RE, (_m, indent, async, name) => `${indent}export ${async || ""}function ${name}(`)
    .replace(
      TOPLEVEL_ARROW_RE,
      (_m, indent, kind, name, async) => `${indent}export ${kind} ${name} = ${async || ""}(`,
    );
  const sink = referenced.size ? `\nvoid [${[...referenced].join(", ")}];\n` : "";

  return "\n".repeat(scriptStart) + exportedScript + sink;
}

export default {
  entry: [
    "librarian/assets/board.html",
    // Knip's own config file is never imported by anything — it's loaded by
    // the knip CLI itself — so without this it reports itself as unused.
    "knip.config.js",
    // One-off, rerunnable maintenance tools invoked manually
    // (`node scripts/reformat-jsdoc.mjs`), not from an npm script or import
    // — see their own header comments. Knip's docs recommend `entry` for
    // exactly this "intentionally manual, never referenced" case.
    "scripts/reformat-jsdoc.mjs",
    // `npm run lint` / `npm run knip` invoke these via npm scripts; knip
    // resolves them through the package.json scripts and needs no entry line
    // for generate-board-globals.mjs or knip-board.mjs.
    // `npm test` (node --test) discovers these by filename, not by import, so
    // knip has no edge into them (RAL-186).
    "test/*.test.mjs",
  ],
  compilers: {
    html: compileHtml,
  },
  includeEntryExports: true,
  ignoreExportsUsedInFile: true,
  // The chunk and vendor files are inlined into the compiled shell above and
  // are never imported by anything — without this knip would report them as
  // unused files. The shell itself is the entry and must NOT be covered by
  // this ignore.
  ignore: ["librarian/assets/board/**", "librarian/assets/vendor/**"],
};
