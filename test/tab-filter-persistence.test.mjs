// Coverage for RAL-461: squad-filter/sort (and review-filter) settings must
// survive switching tabs, and must only ever reset from a hash the user
// actually navigated through (browser back/forward, or a direct link) --
// never as a side effect of an ordinary tab click.
//
// `filters` and `reviewFilters` are persistent module-level `let` bindings in
// board.html; the only code that resets them to their defaults is
// `parseHash()`, and the only callers of `parseHash()` are the `popstate`
// listener and the one-time call at initial page load. `showTab()` (which
// every tab button's `onclick` calls) must never reach `parseHash()`,
// directly or indirectly -- these are text-level wiring assertions against
// the real shipped source, since the property under test is "who calls
// whom," not a pure input/output computation.
//
// Run with `npm test` (node --test). Reuses board-squad-filter.mjs's
// `boardSource` export -- it just reads the whole file, unrelated to the
// squad-filter predicate itself.

import test from "node:test";
import assert from "node:assert/strict";
import { boardSource } from "./board-squad-filter.mjs";

/**
 * Slices one `function name(...) { ... }` declaration's source out of
 * `boardSource`, matching braces so nested blocks don't truncate it early.
 * @param {string} name
 * @returns {string}
 */
function extractFunctionBody(name) {
  const needle = `function ${name}(`;
  const start = boardSource.indexOf(needle);
  assert.ok(start > -1, `function ${name} not found in board.html`);
  const bodyStart = boardSource.indexOf("{", start);
  let depth = 0;
  for (let i = bodyStart; i < boardSource.length; i++) {
    const ch = boardSource[i];
    if (ch === "{") depth += 1;
    else if (ch === "}") {
      depth -= 1;
      if (depth === 0) return boardSource.slice(start, i + 1);
    }
  }
  throw new Error(`function ${name} has no matching closing brace`);
}

test("showTab never resets the task/review filters -- an ordinary tab click must not touch parseHash", () => {
  const fn = extractFunctionBody("showTab");
  assert.doesNotMatch(fn, /parseHash\(/, "showTab must not call parseHash(), directly or as a copy-pasted reset");
  assert.doesNotMatch(fn, /defaultTaskFilters\(\)|defaultReviewFilters\(\)/, "showTab must not reset filters to their defaults");
});

test("every tab button's onclick calls showTab(name, true) -- not a hash-routing path that could re-parse and reset filters", () => {
  const tabButtons = [...boardSource.matchAll(/id="tab-(\w+)"[^>]*onclick="([^"]+)"/g)];
  assert.ok(tabButtons.length > 5, "expected to find the tab button bar in board.html");
  for (const [, name, onclick] of tabButtons) {
    assert.match(onclick, /^showTab\('\w+', true\)$/, `tab-${name}'s onclick must be a plain showTab(name, true) call`);
  }
});

test("parseHash (the only place filters reset to their defaults) is called from just the popstate listener and initial load", () => {
  const calls = [...boardSource.matchAll(/\bparseHash\(\)/g)];
  // One is the `function parseHash() {` declaration itself.
  const callSites = calls.length - 1;
  assert.equal(callSites, 2, "parseHash() must only be invoked from the popstate listener and the one-time initial-load call -- any other call site can silently reset the user's filters on an ordinary navigation");
});

test("filters/reviewFilters are only reassigned to their defaults inside parseHash (excluding their initial `let` declarations)", () => {
  const parseHashFn = extractFunctionBody("parseHash");
  const RESET_RE = /(?<!let )\b(filters|reviewFilters) = default(Task|Review)Filters\(\)/g;
  const resetsInParseHash = (parseHashFn.match(RESET_RE) || []).length;
  const resetsAnywhere = (boardSource.match(RESET_RE) || []).length;
  assert.equal(resetsInParseHash, resetsAnywhere, "a filters/reviewFilters reset outside parseHash() would reset the user's settings on some path other than a real hash navigation");
});
