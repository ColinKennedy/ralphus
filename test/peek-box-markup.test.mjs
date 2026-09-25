// RAL-434 follow-up: the Live View's "Show Thinking" checkbox was built into a
// `thinkingToggleHtml` string that peekBox never interpolated into its returned
// markup, so the control existed in source — tooltip, handler, config default
// and all — and rendered nowhere. Nothing caught it: eslint's unused-variable
// rule does not fire on a const declared inside a live function, the state
// machine tests stop at the DOM boundary, and the daemon-side half (the tagged
// transcript, the config default) was correct.
//
// peekBox needs a document, so it cannot be evaluated here the way
// ./board-peek-state.mjs evaluates the pure state machine. These tests read its
// real shipped source instead and assert the shape that was broken: every
// markup fragment the function assembles has to reach the string it returns.

import test from "node:test";
import assert from "node:assert/strict";
import { boardScript } from "./board-source.mjs";

/**
 * Slices peekBox's source out of the concatenated board chunks. The board's
 * top-level functions are indented six spaces (they were a `<script>` body
 * before the chunk split), so the next such `function` line ends the body.
 * @returns {string} peekBox's source text
 */
function peekBoxSource() {
  const html = boardScript();
  const start = html.indexOf("      function peekBox(");
  assert.notEqual(start, -1, "could not find peekBox in the board chunks — if it was renamed or reindented, update this test rather than deleting it.");
  const end = html.indexOf("\n      function ", start + 1);
  assert.notEqual(end, -1, "could not find the function following peekBox — see above.");
  return html.slice(start, end);
}

test("peekBox renders the Show Thinking toggle it builds", () => {
  const src = peekBoxSource();
  assert.ok(src.includes("thinkingToggleHtml ="), "peekBox no longer builds a Show Thinking toggle");
  assert.ok(src.includes("${thinkingToggleHtml}"), "peekBox builds thinkingToggleHtml but never interpolates it — the checkbox would not render (RAL-434 regression)");
});

test("every markup fragment peekBox builds reaches its returned markup", () => {
  const src = peekBoxSource();
  const declared = [...src.matchAll(/\bconst (\w*Html)\b\s*=/g)].map((m) => m[1]);
  assert.ok(declared.length > 3, `expected peekBox to build several *Html fragments, found ${declared.length}`);
  const orphans = declared.filter((name) => !src.includes(`\${${name}}`));
  assert.deepEqual(orphans, [], `peekBox builds these markup fragments but never interpolates them, so they render nowhere: ${orphans.join(", ")}`);
});

// Asked directly: does the toggle survive on a historical record, or is it a
// live-only control? It is not gated on liveness -- `ended`/`detachedAtMs`
// only pick the head banner, dot and tooltip -- and it must stay that way:
// browsing a finished run is exactly when folding reasoning away is most
// useful, and a control that vanished on completion would read as the
// RAL-434 bug all over again.
test("the Show Thinking toggle is not gated on the pane still being live", () => {
  const src = peekBoxSource();
  const decl = src.split("\n").find((l) => l.includes("thinkingToggleHtml ="));
  assert.ok(decl, "peekBox no longer declares thinkingToggleHtml");
  assert.ok(!/\bended\b/.test(decl), "the Show Thinking toggle became conditional on `ended` -- it must render on a historical record too");
  assert.ok(!/\bdetached\b/.test(decl), "the Show Thinking toggle became conditional on `detached` -- it must render on a detached cell's record too");
});
