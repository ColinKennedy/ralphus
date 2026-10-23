// The review inspector's Notes tab lists deferrals as a fourth note type; an
// ignored deferral files under its own filter value that is off by default.
//
// Run with `npm test` (node --test).

import test from "node:test";
import assert from "node:assert/strict";
import { boardScript } from "./board-source.mjs";

const src = boardScript();

/**
 * Evaluates the Notes filter declarations out of the real board source.
 * @returns {{noteFilterKey: (n: object) => string, options: {value: string}[], filter: Set<string>}}
 */
function load() {
  const opts = src.match(/const NOTE_TYPE_OPTIONS = \[[\s\S]*?\n {6}\];/)[0];
  const fn = src.match(/function noteFilterKey\(n\) \{[\s\S]*?\n {6}\}/)[0];
  const filter = src.match(/const notesTypeFilter = .*;/)[0];
  return new Function(`${opts}\n${fn}\n${filter}\nreturn {noteFilterKey, options: NOTE_TYPE_OPTIONS, filter: notesTypeFilter};`)();
}

test("deferral and ignored-deferral are filter options; ignored is hidden by default", () => {
  const { options, filter } = load();
  const values = options.map((o) => o.value);
  assert.ok(values.includes("deferral"));
  assert.ok(values.includes("deferral_ignored"));
  assert.ok(filter.has("deferral"));
  assert.ok(!filter.has("deferral_ignored"));
});

test("noteFilterKey separates ignored deferrals from the rest", () => {
  const { noteFilterKey } = load();
  assert.equal(noteFilterKey({ type: "deferral", ignored: false }), "deferral");
  assert.equal(noteFilterKey({ type: "deferral", ignored: true }), "deferral_ignored");
  assert.equal(noteFilterKey({ type: "ghost" }), "ghost");
});
