// RAL-434 follow-up: the Live View's "Show Thinking" checkbox was once built into
// a string that the widget never interpolated into its returned markup, so the
// control existed in source — tooltip, handler, config default and all — and
// rendered nowhere. Nothing caught it: eslint's unused-variable rule does not
// fire on a const declared inside a live function.
//
// liveViewWidget needs a document, so it cannot be evaluated here the way
// ./board-peek-state.mjs evaluates the pure state machine. These tests read its
// real shipped source instead and assert the shape that was broken: every
// markup fragment the function assembles has to reach the string it returns.

import test from "node:test";
import assert from "node:assert/strict";
import { boardScript } from "./board-source.mjs";

/**
 * Slices liveViewWidget's source out of the concatenated board chunks. The
 * board's top-level functions are indented six spaces, so the next such
 * `function` line ends the body.
 * @returns {string} liveViewWidget's source text
 */
function widgetSource() {
  const html = boardScript();
  const start = html.indexOf("      function liveViewWidget(");
  assert.notEqual(start, -1, "could not find liveViewWidget in the board chunks — if it was renamed or reindented, update this test rather than deleting it.");
  const end = html.indexOf("\n      function ", start + 1);
  assert.notEqual(end, -1, "could not find the function following liveViewWidget — see above.");
  return html.slice(start, end);
}

test("liveViewWidget renders the Show Thinking toggle it builds", () => {
  const src = widgetSource();
  assert.ok(src.includes('data-click="toggleShowThinkingBtn"'), "liveViewWidget no longer builds a Show Thinking toggle");
  assert.ok(/const top = .*\bctl\b/.test(src), "liveViewWidget builds its control row but never interpolates it — the checkbox would not render (RAL-434 regression)");
});

test("every markup fragment liveViewWidget builds reaches its returned markup", () => {
  const src = widgetSource();
  const declared = [...src.matchAll(/\bconst (\w+)\s*=\s*(?:run\s*\?\s*)?`</g)].map((m) => m[1]);
  assert.ok(declared.length >= 3, `expected liveViewWidget to build several markup fragments, found ${declared.length}`);
  const top = src.slice(src.indexOf("const top ="), src.indexOf("\n", src.indexOf("const top =")));
  const orphans = declared.filter((name) => !top.includes(name) && !src.includes(`\${${name}}`));
  assert.deepEqual(orphans, [], `liveViewWidget builds these markup fragments but never uses them, so they render nowhere: ${orphans.join(", ")}`);
});

// The toggle must stay visible on a historical record: browsing a finished run is
// exactly when folding reasoning away is most useful. Only the context's
// `canThink` may hide it; the run's liveness may not.
test("the Show Thinking toggle is not gated on the pane still being live", () => {
  const src = widgetSource();
  const i = src.indexOf("${ctx.canThink ?");
  assert.notEqual(i, -1, "the Show Thinking toggle is no longer gated on ctx.canThink");
  const line = src.slice(i, src.indexOf("\n", i));
  assert.ok(!/\bended\b|\blive\b|\bdetached\b/.test(line), "the Show Thinking toggle became conditional on liveness -- it must render on a historical record too");
});
