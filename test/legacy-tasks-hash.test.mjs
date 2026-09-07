// Coverage for RAL-362's legacy-redirect requirement: an old `#tasks...`
// link -- back when the squad viewer itself lived at `#/tasks`, before it
// was renamed to `#/squads` -- must still land on the Squads tab rather
// than the new flat Tasks tab.
//
// Run with `npm test` (node --test). See ./board-legacy-tasks-hash.mjs for
// how the detection logic is loaded out of the real board.html.

import test from "node:test";
import assert from "node:assert/strict";
import { legacyTasksHash, boardSource } from "./board-legacy-tasks-hash.mjs";

const { isLegacySquadsTasksHash } = legacyTasksHash;

test("a positional squad id in the path is a legacy link", () => {
  assert.equal(isLegacySquadsTasksHash("tasks/squad-1"), true);
});

test("a bare legacy kind:ti:si selector is a legacy link", () => {
  assert.equal(isLegacySquadsTasksHash("tasks?sel=task:0:1"), true);
});

test("a bare legacy cell/proof selector is also a legacy link", () => {
  assert.equal(isLegacySquadsTasksHash("tasks?sel=cell:0:1"), true);
  assert.equal(isLegacySquadsTasksHash("tasks?sel=proof:0:1:2"), true);
});

test("a URI selector whose first segment is a SQUAD is a legacy link", () => {
  assert.equal(isLegacySquadsTasksHash("tasks?sel=SQUAD[my-squad]"), true);
});

test("a bare #tasks with no path/selector is not a legacy link", () => {
  assert.equal(isLegacySquadsTasksHash("tasks"), false);
});

test("a plain query string with no sel is not a legacy link", () => {
  assert.equal(isLegacySquadsTasksHash("tasks?q=foo&sort=date"), false);
});

test("a new Tasks-tab URI selector (TASK first segment) is not a legacy link", () => {
  assert.equal(isLegacySquadsTasksHash("tasks?sel=TASK[my-task]"), false);
});

test("a new Tasks-tab URI selector with a query is not a legacy link", () => {
  assert.equal(isLegacySquadsTasksHash("tasks?q=foo&sel=TASK[my-task]?id=squad-1"), false);
});

// The assertion below is about wiring rather than pure logic: it reads the
// shipped board.html directly to confirm `parseHash` actually dispatches
// through `isLegacySquadsTasksHash` for the `#tasks...` prefix, since that
// dispatch itself lives in `parseHash` (which touches module-level state)
// and so cannot be evaluated here.

test("parseHash routes #tasks... through isLegacySquadsTasksHash before falling back to the new Tasks tab", () => {
  const body = boardSource.slice(boardSource.indexOf("function parseHash()"));
  const fn = body.slice(0, body.indexOf("\n      }\n") + 1);
  assert.match(fn, /raw\.startsWith\("tasks"\)/);
  assert.match(fn, /isLegacySquadsTasksHash\(raw\)\s*\?\s*parseSquadsHashBody\(raw\)\s*:\s*parseTasksTabHash\(raw\)/);
});
