// Coverage for the Logs modal's cells/proofs-tab "output" button (RAL-295).
//
// Before this change, the proofs tab read a `v.output` field stored on the
// proof view and popped it open with `openCmdPopup`; the cells tab had no
// output button at all. Both tabs now render the same `logsOutputBtn(key,
// state)` button, which fetches its content on demand from the daemon's
// pane/terminal-log-attempts endpoints via `openLinkedOutputPopup` instead of
// reading a pre-stored field. What's pinned here: the empty-state fallback
// for rows that haven't run yet, and that the button carries the right key
// for both cell and proof rows so the click handler resolves it correctly.
//
// Run with `npm test` (node --test). See ./board-logs-output-btn.mjs for how
// the button logic is loaded out of the real board.html.

import test from "node:test";
import assert from "node:assert/strict";
import { logsOutputBtn, boardSource } from "./board-logs-output-btn.mjs";

test("a pending or queued row falls back to the empty-state dash, not a button", () => {
  for (const state of ["pending", "queued"]) {
    assert.equal(logsOutputBtn("cell|squad-abc|0|1", state), "—");
  }
});

test("every other state renders a clickable output button", () => {
  for (const state of ["running", "success", "failed", "cancelled", "detached"]) {
    const html = logsOutputBtn("cell|squad-abc|0|1", state);
    assert.notEqual(html, "—", state);
    assert.match(html, /<button/, state);
    assert.match(html, />▶ output<\/button>/, state);
  }
});

test("the button dispatches through the openLinkedOutputPopup click handler", () => {
  const html = logsOutputBtn("cell|squad-abc|0|1", "success");
  assert.match(html, /data-click="openLinkedOutputPopup"/);
});

test("the button carries a cell row's key unchanged", () => {
  const html = logsOutputBtn("cell|squad-abc|2|3", "success");
  assert.match(html, /data-key="cell\|squad-abc\|2\|3"/);
});

test("the button carries a task-scoped proof row's key unchanged", () => {
  const html = logsOutputBtn("proof|squad-abc|0|task|-1|2", "failed");
  assert.match(html, /data-key="proof\|squad-abc\|0\|task\|-1\|2"/);
});

test("the button carries a cell-scoped proof row's key unchanged", () => {
  const html = logsOutputBtn("proof|squad-abc|0|cell|1|2", "success");
  assert.match(html, /data-key="proof\|squad-abc\|0\|cell\|1\|2"/);
});

test("the button ships with a data-tip explaining it fetches live, not a stored summary", () => {
  const html = logsOutputBtn("cell|squad-abc|0|1", "success");
  assert.match(html, /data-tip="/);
  assert.match(html, /not a stored summary/);
});

// ---- Wiring: the cells and proofs tabs both use this button ----
//
// These assertions read the shipped board.html directly rather than
// evaluating it, because cumulativeCellRows/logsBody need a SquadView and are
// not part of the deliberately-pure marker region above.

test("the cells tab renders an output column via logsOutputBtn, keyed by task/cell index", () => {
  assert.match(boardSource, /logsOutputBtn\(`cell\|\$\{squad\.id\}\|\$\{ti\}\|\$\{si\}`, s\.state\)/);
});

test("the cells tab table header includes an output column", () => {
  assert.match(
    boardSource,
    /return T\(\["task", "cell", "state", "tokens \(this squad\)", "cost \(this squad\)", cumHead, "output", "error"\], cumulativeCellRows\(squad\)\);/,
  );
});

test("the proofs tab is migrated off the old v.output/data-full popup onto logsOutputBtn", () => {
  const fnStart = boardSource.indexOf('if (logsTab === "proofs")');
  const fnBody = boardSource.slice(fnStart, boardSource.indexOf("return rows.length", fnStart));
  // Only the RAL-295 migration comment is allowed to mention the old field by
  // name; strip comment lines before checking the code itself moved on.
  const codeOnly = fnBody.replace(/^\s*\/\/.*$/gm, "");
  assert.doesNotMatch(codeOnly, /v\.output/, "the proofs tab must no longer read a stored output field");
  assert.doesNotMatch(codeOnly, /data-full/, "the proofs tab must no longer use the old synchronous popup");
  assert.doesNotMatch(codeOnly, /openCmdPopup/, "the proofs tab must no longer use the old synchronous popup");
  assert.match(fnBody, /logsOutputBtn\(`proof\|\$\{squad\.id\}\|\$\{ti\}\|task\|-1\|\$\{vi\}`, v\.state\)/, "task-level proof rows must use logsOutputBtn");
  assert.match(fnBody, /logsOutputBtn\(`proof\|\$\{squad\.id\}\|\$\{ti\}\|cell\|\$\{si\}\|\$\{vi\}`, v\.state\)/, "cell-level proof rows must use logsOutputBtn");
});

test("the click handler for the output button is registered", () => {
  assert.match(boardSource, /CLICK_HANDLERS\.openLinkedOutputPopup = \(e, ds\) => openLinkedOutputPopup\(ds\.key \|\| ""\);/);
});
