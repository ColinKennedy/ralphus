// Coverage for RAL-436: `renderAll` (the Squads tab's shared sidebar+graph+
// details refresh, called from both `pollTasks` and every pushed-event
// refresh) must only ever skip the *details pane* while `editing` or a live
// in-pane selection is active -- never the sidebar or graph. Before this fix,
// `pollTasks` skipped calling `renderAll` at all while `editing`, which froze
// the whole dependency graph (every status pill, cancellation, restart, and
// proof badge) until whatever edit form was open anywhere on the page closed.
//
// Run with `npm test` (node --test). See ./board-preserve-scroll.mjs for how
// the regions are loaded out of the real board chunks.

import test from "node:test";
import assert from "node:assert/strict";
import { makeRenderAll, FakeNode, FakeInput } from "./board-preserve-scroll.mjs";

test("renderAll: with nothing editing or selected, the sidebar, graph, and details pane all render", () => {
  const ra = makeRenderAll({ editing: false });
  ra.document.register(new FakeNode("details"));
  ra.renderAll();
  assert.equal(ra.calls.renderSquads, 1);
  assert.equal(ra.calls.renderGraph, 1);
  assert.equal(ra.calls.renderDetails, 1);
});

test("renderAll: while editing, the sidebar and graph still render -- only the details pane is skipped", () => {
  const ra = makeRenderAll({ editing: true });
  ra.document.register(new FakeNode("details"));
  ra.renderAll();
  assert.equal(ra.calls.renderSquads, 1, "the sidebar must keep updating live while an edit form is open");
  assert.equal(ra.calls.renderGraph, 1, "the dependency graph must keep updating live while an edit form is open");
  assert.equal(ra.calls.renderDetails, 0, "the details pane must not be rebuilt out from under the open edit form");
});

test("renderAll: with a live in-pane selection (RAL-431), the sidebar and graph still render -- only the details pane is skipped", () => {
  const ra = makeRenderAll({ editing: false });
  const details = new FakeNode("details");
  const input = new FakeInput("some-field", "hello");
  details.appendChild(input);
  ra.document.register(details);
  input.selectionStart = 0;
  input.selectionEnd = 3; // a non-collapsed selection
  ra.document.setActive(input);
  ra.renderAll();
  assert.equal(ra.calls.renderSquads, 1);
  assert.equal(ra.calls.renderGraph, 1);
  assert.equal(ra.calls.renderDetails, 0, "must not wipe the user's in-pane text selection");
});
