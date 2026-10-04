// Coverage for the review sections' "Run all": both manual checks and test
// actions launch every ready check through the per-row path, so each launched
// row tracks its own status, duration and output. `runnableCheckIndexes` is
// sliced from the real board chunks between the RALPHUS-RUN-ALL markers.
//
// Run with `npm test` (node --test).

import test from "node:test";
import assert from "node:assert/strict";
import { boardScript } from "./board-source.mjs";

const BEGIN = "// RALPHUS-RUN-ALL:BEGIN";
const END = "// RALPHUS-RUN-ALL:END";
const html = boardScript();
const from = html.indexOf(BEGIN);
const to = html.indexOf(END);
if (from === -1 || to === -1 || to < from) {
  throw new Error(`run-all-checks: could not find the ${BEGIN} / ${END} markers in the board chunks.`);
}
// eslint-disable-next-line no-new-func -- evaluating the real shipped source is the point.
const { runnableCheckIndexes } = new Function(`${html.slice(from + BEGIN.length, to)}\nreturn { runnableCheckIndexes };`)();

/** The body of one named board function, for assertions about its wiring. */
function functionBody(name) {
  const start = html.indexOf(`async function ${name}(`);
  assert.notEqual(start, -1, `${name} not found`);
  const next = html.indexOf("\n      }\n", start);
  return html.slice(start, next);
}

test("only ready checks with a command are run", () => {
  const checks = [
    { command: "a", preparation_state: "ready" },
    { command: "b", preparation_state: "preparing" },
    { prompt: "c", preparation_state: "ready" },
    { command: "d", preparation_state: "ready", inputs: [{ name: "port" }] },
    { command: "e" },
  ];
  assert.deepEqual(runnableCheckIndexes(checks), [0, 3]);
});

test("nothing runnable yields no indexes", () => {
  assert.deepEqual(runnableCheckIndexes([]), []);
  assert.deepEqual(runnableCheckIndexes([{ command: "x", preparation_state: "failed" }]), []);
});

test("manual checks' Run all launches each row through runCheck, not the bulk route", () => {
  const body = functionBody("runAllManualChecks");
  assert.match(body, /runnableCheckIndexes\(g\.manual_commands/);
  assert.match(body, /runCheck\("manual", id, i\)/);
  assert.doesNotMatch(body, /guardianAction\(/, "a bulk request marks no row launched, so no row would track its run");
});

test("test actions' Run all uses the same selection", () => {
  const body = functionBody("runAllActionHints");
  assert.match(body, /runnableCheckIndexes\(g\.action_hints/);
  assert.match(body, /runCheck\("action", id, i\)/);
});

test("a refused launch returns its row to idle instead of leaving it running", () => {
  const body = functionBody("runCheckWithInputs");
  assert.match(body, /if \(!resp \|\| !resp\.ok\) forgetCommandLaunch\(key\)/);
});
