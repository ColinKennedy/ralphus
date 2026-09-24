// Guards the Tasks tab's column model against the two ways a column can be
// declared but never actually reach the user: a header with no tooltip, and a
// column track with no cell rendered into it (which silently shifts every
// later column one track left, because `ttColCell` emits grid children
// positionally). The Turns column shipped with both faults before RAL-40's
// tooltip sweep caught them.
//
// Run with `npm test` (node --test).

import test from "node:test";
import assert from "node:assert/strict";
import { taskTabLogic } from "./board-task-tab-logic.mjs";
import { boardScript } from "./board-source.mjs";

const { TASK_TAB_COLUMNS, TT_COLUMN_TIPS } = taskTabLogic;

/** The three row shapes the Tasks tab renders: task, expanded cell, group header. */
const ROW_RENDERERS = 3;

test("every Tasks tab column has a header tooltip", () => {
  for (const c of TASK_TAB_COLUMNS) {
    const tip = TT_COLUMN_TIPS[c.key];
    assert.ok(
      typeof tip === "string" && tip.trim().length > 0,
      `column "${c.key}" has no TT_COLUMN_TIPS entry — its header hovers to an empty tooltip`,
    );
  }
});

test("every Tasks tab column is rendered by all three row renderers", () => {
  const source = boardScript();
  for (const c of TASK_TAB_COLUMNS) {
    const uses = source.split(`ttColCell("${c.key}"`).length - 1;
    assert.equal(
      uses,
      ROW_RENDERERS,
      `column "${c.key}" is rendered by ${uses} of ${ROW_RENDERERS} row renderers — ` +
        "ttTaskRowHtml, ttCellRowHtml and ttGroupHeaderHtml must each emit a cell for it, " +
        "or the grid tracks after it shift one column left",
    );
  }
});

test("Turns is a declared, sortable, hideable column shown by default", () => {
  const turns = TASK_TAB_COLUMNS.find((c) => c.key === "turns");
  assert.ok(turns, "the Turns column is missing from TASK_TAB_COLUMNS");
  assert.equal(turns.label, "Turns");
  assert.equal(turns.sortable, true);
  assert.equal(turns.hideable, true);
  assert.ok(
    !taskTabLogic.TASK_TAB_DEFAULT_HIDDEN_COLS.includes("turns"),
    "Turns is meant to be visible in the default view",
  );
});
