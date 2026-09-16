// Coverage for RAL-448 "Restore Tasks tab multi-selection and resizing":
//
// - ttVisibleTaskKeys only ever lists rows the current filter/group/sort
//   pass actually produced (group-header and expanded-cell entries excluded);
// - a plain click on a row selects just that row, clearing any prior
//   multi-selection and becoming the shift-range anchor;
// - a ctrl/cmd-click toggles just the clicked row's own membership without
//   touching the rest of the selection;
// - a shift-click range-selects between the last anchor and the clicked row
//   (among visible rows only), replacing any prior selection -- with no
//   anchor yet, it falls back to a plain single selection;
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

// ---------- ttToggleRowSel ----------

test("a plain click selects just that row and becomes the shift-range anchor", () => {
  const h = makeMultiSelect({ ttDisplayItems: taskDisplayItems(5) });
  h.ttToggleRowSel({ shiftKey: false, ctrlKey: false, metaKey: false }, "sq-2", 0);
  assert.deepEqual([...h.state().ttSel], ["sq-2:0"]);
  assert.equal(h.state().ttSelAnchor, "sq-2:0");
  assert.equal(h.calls.renderTasksTab, 1);
});

test("a plain click clears any prior multi-selection down to just the clicked row", () => {
  const h = makeMultiSelect({ ttDisplayItems: taskDisplayItems(5), ttSel: new Set(["sq-0:0", "sq-1:0", "sq-4:0"]) });
  h.ttToggleRowSel({ shiftKey: false, ctrlKey: false, metaKey: false }, "sq-2", 0);
  assert.deepEqual([...h.state().ttSel], ["sq-2:0"]);
});

test("a ctrl/cmd-click toggles just the clicked row on, leaving the rest of the selection untouched", () => {
  const h = makeMultiSelect({ ttDisplayItems: taskDisplayItems(5), ttSel: new Set(["sq-0:0"]) });
  h.ttToggleRowSel({ shiftKey: false, ctrlKey: true, metaKey: false }, "sq-2", 0);
  assert.deepEqual([...h.state().ttSel].sort(), ["sq-0:0", "sq-2:0"]);
  assert.equal(h.state().ttSelAnchor, "sq-2:0");
});

test("a ctrl/cmd-click on an already-selected row toggles it off, leaving the rest of the selection untouched", () => {
  const h = makeMultiSelect({ ttDisplayItems: taskDisplayItems(5), ttSel: new Set(["sq-0:0", "sq-2:0"]) });
  h.ttToggleRowSel({ shiftKey: false, ctrlKey: false, metaKey: true }, "sq-2", 0);
  assert.deepEqual([...h.state().ttSel], ["sq-0:0"]);
});

test("shift-click with no prior anchor falls back to a plain single selection", () => {
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

test("shift-click range selection replaces, rather than adds to, any pre-existing selection", () => {
  const h = makeMultiSelect({ ttDisplayItems: taskDisplayItems(6), ttSel: new Set(["sq-0:0"]), ttSelAnchor: "sq-1:0" });
  h.ttToggleRowSel({ shiftKey: true }, "sq-3", 0);
  assert.deepEqual([...h.state().ttSel].sort(), ["sq-1:0", "sq-2:0", "sq-3:0"]);
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
