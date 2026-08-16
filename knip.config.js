// Knip config for the librarian's inline board.html JavaScript.
//
// board.html is a single static page whose entire client is one inline
// <script> block (see eslint.config.mjs / tsconfig.board.json for the other
// two lint layers). Knip only parses .js/.ts-family files, so board.html
// can't be handed to it unmodified — but unlike the ESLint/tsc setup, it
// doesn't need a separate physical extraction file + path-rewritten output
// (scripts/extract-board-js.mjs + scripts/typecheck-board.mjs): knip's
// `compilers` hook lets us transform a file's content in memory right before
// parsing, so knip still runs against, and reports diagnostics against, the
// real librarian/assets/board.html path — this *is* "using the .html as-is"
// as far as knip's own extension model allows.
//
// board.html's script is a plain global script, not an ES module — no
// import/export — so out of the box knip's cross-file reachability graph has
// nothing to walk inside it: it can only tell whether board.html itself is
// reachable, not whether any function *inside* it is dead. compileHtml()
// below makes knip see intra-file dead code too, by faking a module graph
// over the single file:
//
//   1. Every top-level `function foo(...)` / `async function foo(...)` /
//      `const foo = (...) => ...` is rewritten to carry `export`, so it
//      enters knip's exports analysis at all (`includeEntryExports: true`
//      is required alongside this — entry-file exports are otherwise
//      presumed to be a public API and skipped).
//   2. Every OTHER occurrence of `declaredName(` anywhere in the raw
//      document (i.e. excluding each function's own declaration site) is
//      collected into a synthetic `void [...]` reference sink appended to
//      the compiled output. This is the crux: most of board.html's handlers
//      are wired via inline HTML on* attributes or dynamically-built
//      `onclick="...foo(...)"` strings inside JS template literals — plain
//      text as far as any parser is concerned, invisible to real reference
//      tracking (the same reason ESLint's `no-unused-vars` is disabled
//      below). The sink turns each such name into one real, trivial AST
//      reference, so knip's reference finder counts it as used without
//      needing to understand HTML or string contents at all.
//   3. `ignoreExportsUsedInFile: true` is also required: by default knip
//      only counts an export as "used" if some *other* file imports it — a
//      same-file reference (exactly what the sink produces) is otherwise
//      still reported unused, since exporting normally signals "meant to be
//      imported elsewhere". With this flag, same-file usage counts too.
//
// Net effect: a name left with zero occurrences anywhere in the document
// other than its own declaration — not in real code, not inside any
// on*="..." string, not passed through a helper like
// `terminalMenuItem(key, label, \`foo(...)\`, tip)` — is genuinely dead and
// gets reported. Verified against board.html: found 3 true positives
// (openVerifyNodeMenu, setVerifyMidResolution, queueStabilize — all
// superseded/orphaned handlers with zero other references) and zero false
// positives after the sink was in place, including for functions wired
// through several layers of string indirection.
//
// Caveats — this is a heuristic, not real reference tracking, and can in
// principle miss a genuinely dead function (a false negative, which is
// safe) if:
//   - it's dispatched via fully-dynamic lookup, e.g. `window[name]()` or a
//     dispatch table, where the literal name string never appears next to
//     an opening paren; or
//   - its name happens to coincide with some unrelated call elsewhere in
//     the document (e.g. a same-named property/method on a different
//     object) — low risk given this codebase's descriptive, distinctive
//     naming.
// It will not produce a false positive from any of the string-based wiring
// patterns above, since those are exactly what the sink is built to catch.
const SCRIPT_RE = /<script>\r?\n([\s\S]*?)<\/script>/;
const TOPLEVEL_FN_RE = /^(\s{6})(async\s+)?function\s+([A-Za-z_$][\w$]*)\s*\(/gm;
const TOPLEVEL_ARROW_RE = /^(\s{6})(const|let)\s+([A-Za-z_$][\w$]*)\s*=\s*(async\s+)?\(/gm;
const CALL_RE = /\b([A-Za-z_$][\w$]*)\s*\(/g;

/**
 * Extracts board.html's inline <script>, rewrites every top-level
 * function/arrow declaration into an export, and appends a synthetic
 * reference sink so string-only-dispatched handlers don't false-positive as
 * unused (see the file header for the full rationale).
 * @param {string} text raw board.html source
 * @returns {string} compiled pseudo-module, blank-padded to board.html's line numbers
 */
function compileHtml(text) {
  const match = text.match(SCRIPT_RE);
  if (!match) return "";
  const startLine = text.slice(0, match.index).split("\n").length;
  const script = match[1];

  const declared = new Set();
  for (const m of script.matchAll(TOPLEVEL_FN_RE)) declared.add(m[3]);
  for (const m of script.matchAll(TOPLEVEL_ARROW_RE)) declared.add(m[3]);

  // Blank out each declaration's own header so it can't match CALL_RE and
  // count as a reference to itself.
  const DECL_RE = /(\s{6})(async\s+)?function\s+([A-Za-z_$][\w$]*)\s*\(/g;
  const scanText = text.replace(DECL_RE, (m) => " ".repeat(m.length));

  const referenced = new Set();
  for (const call of scanText.matchAll(CALL_RE)) {
    if (declared.has(call[1])) referenced.add(call[1]);
  }

  const exportedScript = script
    .replace(TOPLEVEL_FN_RE, (_m, indent, async, name) => `${indent}export ${async || ""}function ${name}(`)
    .replace(
      TOPLEVEL_ARROW_RE,
      (_m, indent, kind, name, async) => `${indent}export ${kind} ${name} = ${async || ""}(`,
    );
  const sink = referenced.size ? `\nvoid [${[...referenced].join(", ")}];\n` : "";

  return "\n".repeat(startLine) + exportedScript + sink;
}

export default {
  entry: [
    "librarian/assets/board.html",
    // Knip's own config file is never imported by anything — it's loaded by
    // the knip CLI itself — so without this it reports itself as unused.
    "knip.config.js",
    // A one-off, rerunnable maintenance tool invoked manually
    // (`node scripts/reformat-jsdoc.mjs`), not from an npm script or import
    // — see its own header comment. Knip's docs recommend `entry` for
    // exactly this "intentionally manual, never referenced" case.
    "scripts/reformat-jsdoc.mjs",
    // `npm test` (node --test) discovers these by filename, not by import, so
    // knip has no edge into them (RAL-186).
    "test/*.test.mjs",
  ],
  compilers: {
    html: compileHtml,
  },
  includeEntryExports: true,
  ignoreExportsUsedInFile: true,
  // No `project` override: knip's default project glob
  // (**/*.{js,mjs,cjs,jsx,ts,tsx,mts,cts}, gitignore-filtered) already covers
  // every other file in this Node package (eslint.config.mjs,
  // scripts/*.mjs), so it also audits those for unused files/exports/deps —
  // not just board.html.
};
