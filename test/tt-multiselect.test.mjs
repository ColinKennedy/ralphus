// Coverage for RAL-350 "Multi-row selection in the Task table, with
// meatball-menu support":
//
// - ttVisibleTaskKeys only ever lists rows the current filter/group/sort
//   pass actually produced (group-header and expanded-cell entries excluded);
// - the header select-all checkbox's checked/indeterminate state is derived
//   purely from how many of those visible rows are selected;
// - toggling it only ever touches currently visible rows -- a selected row a
//   filter is hiding is left alone, the "double filter" rule;
// - a row's own checkbox toggles it, and shift-click range-selects between
//   the last-clicked row and the shift-clicked one, among visible rows only;
// - ttVisibleSelectedRows (what a bulk action actually reaches) returns just
//   the clicked row when it isn't part of a multi-selection, and the visible
//   subset of the whole selection when it is -- selecting, filtering it down,
//   then widening the filter back restores the full original selection.
//
// Run with `npm test` (node --test). See ./board-tt-multiselect.mjs for how
// the region is loaded out of the real board chunks.

import test from "node:test";
import assert from "node:assert/strict";
import { makeMultiSelect } from "./board-tt-multiselect.mjs";

/** A flat (ungrouped) `ttDisplayItems`-shaped list of `n` task rows, keyed `sq-<i>:0`. */
function taskDisplayItems(n) {
  return Array.from({ length: n }, (_, i) => ({ type: "task", row: { key: `sq-${i}:0`, squadId: `sq-${i}`, taskIdx: 0 } }));
}

/** The `ttAllRows`-shaped counterpart of `taskDisplayItems`, unaffected by any filter. */
function allRows(n) {
  return Array.from({ length: n }, (_, i) => ({ key: `sq-${i}:0`, squadId: `sq-${i}`, taskIdx: 0 }));
}

// ---------- ttVisibleTaskKeys ----------

test("ttVisibleTaskKeys lists only task-type display items, in order, skipping group headers", () => {
  const h = makeMultiSelect({
    ttDisplayItems: [
      { type: "group", squadId: "sq-0", rows: [] },
      { type: "task", row: { key: "sq-0:0" } },
      { type: "cell", row: { key: "sq-0:0" }, cell: {} },
      { type: "task", row: { key: "sq-0:1" } },
    ],
  });
  assert.deepEqual(h.ttVisibleTaskKeys(), ["sq-0:0", "sq-0:1"]);
});

// ---------- ttSelectAllState ----------

test("ttSelectAllState: no visible rows", () => {
  const h = makeMultiSelect({ ttDisplayItems: [] });
  assert.deepEqual(h.ttSelectAllState(), { checked: false, indeterminate: false });
});

test("ttSelectAllState: none of the visible rows selected", () => {
  const h = makeMultiSelect({ ttDisplayItems: taskDisplayItems(3), ttSel: new Set() });
  assert.deepEqual(h.ttSelectAllState(), { checked: false, indeterminate: false });
});

test("ttSelectAllState: some of the visible rows selected", () => {
  const h = makeMultiSelect({ ttDisplayItems: taskDisplayItems(3), ttSel: new Set(["sq-1:0"]) });
  assert.deepEqual(h.ttSelectAllState(), { checked: false, indeterminate: true });
});

test("ttSelectAllState: every visible row selected", () => {
  const h = makeMultiSelect({ ttDisplayItems: taskDisplayItems(3), ttSel: new Set(["sq-0:0", "sq-1:0", "sq-2:0"]) });
  assert.deepEqual(h.ttSelectAllState(), { checked: true, indeterminate: false });
});

test("ttSelectAllState ignores a selected key a filter is currently hiding", () => {
  // sq-9:0 is selected but not among the (filtered) visible rows -- it must
  // not count toward "every visible row selected".
  const h = makeMultiSelect({ ttDisplayItems: taskDisplayItems(2), ttSel: new Set(["sq-0:0", "sq-1:0", "sq-9:0"]) });
  assert.deepEqual(h.ttSelectAllState(), { checked: true, indeterminate: false });
});

// ---------- ttToggleSelectAllVisible ----------

test("ttToggleSelectAllVisible(true) selects every currently visible row and re-renders", () => {
  const h = makeMultiSelect({ ttDisplayItems: taskDisplayItems(3) });
  h.ttToggleSelectAllVisible(true);
  assert.deepEqual([...h.state().ttSel].sort(), ["sq-0:0", "sq-1:0", "sq-2:0"]);
  assert.equal(h.state().ttSelAnchor, null);
  assert.equal(h.calls.renderTasksTab, 1);
});

test("ttToggleSelectAllVisible(false) only deselects visible rows -- a selection a filter hides survives (double filter)", () => {
  const h = makeMultiSelect({
    ttDisplayItems: taskDisplayItems(2), // only sq-0:0, sq-1:0 visible under the current filter
    ttSel: new Set(["sq-0:0", "sq-1:0", "sq-9:0"]),
  });
  h.ttToggleSelectAllVisible(false);
  assert.deepEqual([...h.state().ttSel], ["sq-9:0"]);
});

// ---------- ttToggleRowSel ----------

test("a plain click toggles just that row on and becomes the shift-range anchor", () => {
  const h = makeMultiSelect({ ttDisplayItems: taskDisplayItems(5) });
  h.ttToggleRowSel({ shiftKey: false }, "sq-2", 0);
  assert.deepEqual([...h.state().ttSel], ["sq-2:0"]);
  assert.equal(h.state().ttSelAnchor, "sq-2:0");
  assert.equal(h.calls.renderTasksTab, 1);
});

test("a plain click on an already-selected row toggles it back off", () => {
  const h = makeMultiSelect({ ttDisplayItems: taskDisplayItems(5), ttSel: new Set(["sq-2:0"]) });
  h.ttToggleRowSel({ shiftKey: false }, "sq-2", 0);
  assert.deepEqual([...h.state().ttSel], []);
});

test("shift-click with no prior anchor just toggles the clicked row (and becomes the anchor)", () => {
  const h = makeMultiSelect({ ttDisplayItems: taskDisplayItems(5) });
  h.ttToggleRowSel({ shiftKey: true }, "sq-3", 0);
  assert.deepEqual([...h.state().ttSel], ["sq-3:0"]);
  assert.equal(h.state().ttSelAnchor, "sq-3:0");
});

test("shift-click with an anchor selects the contiguous visible range between them", () => {
  const h = makeMultiSelect({ ttDisplayItems: taskDisplayItems(6), ttSelAnchor: "sq-1:0" });
  h.ttToggleRowSel({ shiftKey: true }, "sq-4", 0);
  assert.deepEqual([...h.state().ttSel].sort(), ["sq-1:0", "sq-2:0", "sq-3:0", "sq-4:0"]);
});

test("shift-click range selection works in either direction", () => {
  const h = makeMultiSelect({ ttDisplayItems: taskDisplayItems(6), ttSelAnchor: "sq-4:0" });
  h.ttToggleRowSel({ shiftKey: true }, "sq-1", 0);
  assert.deepEqual([...h.state().ttSel].sort(), ["sq-1:0", "sq-2:0", "sq-3:0", "sq-4:0"]);
});

test("shift-click range selection adds to, rather than replaces, any pre-existing selection", () => {
  const h = makeMultiSelect({ ttDisplayItems: taskDisplayItems(6), ttSel: new Set(["sq-0:0"]), ttSelAnchor: "sq-1:0" });
  h.ttToggleRowSel({ shiftKey: true }, "sq-3", 0);
  assert.deepEqual([...h.state().ttSel].sort(), ["sq-0:0", "sq-1:0", "sq-2:0", "sq-3:0"]);
});

// ---------- ttVisibleSelectedRows ----------

test("a row that isn't part of a multi-selection resolves to just itself", () => {
  const h = makeMultiSelect({ ttAllRows: allRows(3), ttDisplayItems: taskDisplayItems(3), ttSel: new Set() });
  assert.deepEqual(h.ttVisibleSelectedRows("sq-1:0"), [{ key: "sq-1:0", squadId: "sq-1", taskIdx: 0 }]);
});

test("a lone selected row (selection size 1) also resolves to just itself", () => {
  const h = makeMultiSelect({ ttAllRows: allRows(3), ttDisplayItems: taskDisplayItems(3), ttSel: new Set(["sq-1:0"]) });
  assert.deepEqual(h.ttVisibleSelectedRows("sq-1:0"), [{ key: "sq-1:0", squadId: "sq-1", taskIdx: 0 }]);
});

test("a row in a multi-row selection resolves to every selected row currently visible", () => {
  const h = makeMultiSelect({
    ttAllRows: allRows(5),
    ttDisplayItems: taskDisplayItems(5),
    ttSel: new Set(["sq-0:0", "sq-2:0", "sq-4:0"]),
  });
  const got = h.ttVisibleSelectedRows("sq-2:0").map((r) => r.key).sort();
  assert.deepEqual(got, ["sq-0:0", "sq-2:0", "sq-4:0"]);
});

test("double filter: a bulk action only reaches the selected rows a filter still shows", () => {
  const selected = new Set(["sq-0:0", "sq-1:0", "sq-2:0"]);
  // The filter is currently narrowed to just sq-1 -- sq-0 and sq-2 are
  // selected but hidden by it.
  const h = makeMultiSelect({
    ttAllRows: allRows(3),
    ttDisplayItems: [{ type: "task", row: { key: "sq-1:0", squadId: "sq-1", taskIdx: 0 } }],
    ttSel: selected,
  });
  const got = h.ttVisibleSelectedRows("sq-1:0").map((r) => r.key);
  assert.deepEqual(got, ["sq-1:0"]);
  // The underlying selection itself is untouched by having been narrowed.
  assert.deepEqual([...h.state().ttSel].sort(), ["sq-0:0", "sq-1:0", "sq-2:0"]);
});

test("double filter: widening the filter back out restores the full original selection", () => {
  const selected = new Set(["sq-0:0", "sq-1:0", "sq-2:0"]);
  const h = makeMultiSelect({ ttAllRows: allRows(3), ttDisplayItems: taskDisplayItems(3), ttSel: selected });
  const got = h.ttVisibleSelectedRows("sq-1:0").map((r) => r.key).sort();
  assert.deepEqual(got, ["sq-0:0", "sq-1:0", "sq-2:0"]);
});
