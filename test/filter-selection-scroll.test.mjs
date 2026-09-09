// Coverage for RAL-383 "Preserve Selected Task After Clearing Filters":
//
// - each Tasks-tab filter setter (name query, show-hidden, needs-me, one
//   status, all/none status) re-renders and then re-centers the retained
//   selection, in that order;
// - the centering math places a selection far outside the initial viewport
//   back in view, computed from `ttDisplayItems`' position rather than a DOM
//   query (so it works even though the row wasn't mounted before the filter
//   widened the result set);
// - a selection that isn't (or is no longer) present in the filtered/grouped
//   display list is left alone -- no exception, no scroll;
// - the regular background poll path does not re-center the selection, only
//   the hash-restore branch does, so routine refreshes can't fight the user's
//   own scrolling.
//
// Run with `npm test` (node --test). See ./board-filter-selection-scroll.mjs
// for how the regions are loaded out of the real board chunks.

import test from "node:test";
import assert from "node:assert/strict";
import { boardSource, makeFilterScroll } from "./board-filter-selection-scroll.mjs";

/** A flat (ungrouped) `ttDisplayItems`-shaped list of `n` task rows, `sq-<i>`/taskIdx 0. */
function taskDisplayItems(n) {
  return Array.from({ length: n }, (_, i) => ({ type: "task", row: { squadId: `sq-${i}`, taskIdx: 0 } }));
}

// ---------- centering math: selection far outside the initial viewport ----------

test("clearing the name filter re-centers a selection far below the initial viewport", () => {
  const items = taskDisplayItems(200);
  const h = makeFilterScroll({
    taskTabSel: { kind: "task", squadId: "sq-150", taskIdx: 0, cellIdx: -1 },
    ttDisplayItems: items,
    scrollTop: 0,
    clientHeight: 300,
    headHeight: 32,
    rowH: 32,
  });
  h.ttSetNameFilter("");
  // idx 150: targetTop = 32 (head) + 150*32 = 4832; centered = 4832 - 300/2 + 32/2
  assert.equal(h.wrap.scrollTop, 4832 - 150 + 16);
  assert.ok(h.wrap.scrollTop > h.wrap.clientHeight, "scrolled well past the initial viewport");
});

test("re-centering recomputes the row's position from ttDisplayItems, not a stale DOM query", () => {
  const items = taskDisplayItems(10);
  const h = makeFilterScroll({
    taskTabSel: { kind: "task", squadId: "sq-9", taskIdx: 0, cellIdx: -1 },
    ttDisplayItems: items,
    scrollTop: 500, // starts scrolled somewhere unrelated
    clientHeight: 100,
    headHeight: 32,
    rowH: 32,
  });
  h.ttToggleShowHidden(true);
  const targetTop = 32 + 9 * 32;
  assert.equal(h.wrap.scrollTop, Math.max(0, targetTop - 50 + 16));
});

// ---------- wiring: render, then re-center, then sync the hash ----------

test("each filter setter renders, re-centers the selection, then syncs the hash, in that order", () => {
  const cases = [
    ["ttSetNameFilter", ["x"]],
    ["ttToggleShowHidden", [true]],
    ["ttToggleNeedsMe", [true]],
    ["ttToggleStatus", ["running", true]],
  ];
  for (const [name, args] of cases) {
    const h = makeFilterScroll({
      taskTabSel: { kind: "task", squadId: "sq-3", taskIdx: 0, cellIdx: -1 },
      ttDisplayItems: taskDisplayItems(10),
    });
    h[name](...args);
    // renderTasksTab first, then the scroll's own ttRenderVisibleRows, then syncHash
    assert.deepEqual(h.calls.order, ["renderTasksTab", "ttRenderVisibleRows", "syncHash"], `${name} call order`);
  }
});

test("ttAllStatus rebuilds the status checkboxes before rendering and re-centering", () => {
  const h = makeFilterScroll({
    taskTabSel: { kind: "task", squadId: "sq-1", taskIdx: 0, cellIdx: -1 },
    ttDisplayItems: taskDisplayItems(5),
  });
  h.ttAllStatus(true);
  assert.deepEqual(h.calls.order, ["renderTtStatusFilters", "renderTasksTab", "ttRenderVisibleRows", "syncHash"]);
  assert.deepEqual(h.state().taskTabFilters.status, new Set(["queued", "running", "done", "failed", "ignored"]));
});

// ---------- filter state mutation (each setter still does its own job) ----------

test("ttSetNameFilter lowercases and stores the query", () => {
  const h = makeFilterScroll({ ttDisplayItems: [] });
  h.ttSetNameFilter("Deploy Task");
  assert.equal(h.state().taskTabFilters.q, "deploy task");
});

test("ttToggleStatus adds/removes one state from the filter set", () => {
  const h = makeFilterScroll({ ttDisplayItems: [] });
  h.ttToggleStatus("done", true);
  assert.ok(h.state().taskTabFilters.status.has("done"));
  h.ttToggleStatus("done", false);
  assert.ok(!h.state().taskTabFilters.status.has("done"));
});

// ---------- no-op when the selection doesn't survive/isn't in the display list ----------

test("a selection absent from ttDisplayItems (filtered out, or nothing selected) is left alone -- no scroll, no throw", () => {
  const h = makeFilterScroll({
    taskTabSel: { kind: "task", squadId: "sq-not-shown", taskIdx: 0, cellIdx: -1 },
    ttDisplayItems: taskDisplayItems(10),
    scrollTop: 42,
  });
  assert.doesNotThrow(() => h.ttToggleNeedsMe(true));
  assert.equal(h.wrap.scrollTop, 42, "scrollTop untouched when the selection isn't in the display list");
  assert.equal(h.calls.ttRenderVisibleRows, 0, "the scroll helper's own re-render never ran");
});

test("no selection at all is a no-op for the scroll step", () => {
  const h = makeFilterScroll({
    taskTabSel: { kind: null, squadId: null, taskIdx: -1, cellIdx: -1 },
    ttDisplayItems: taskDisplayItems(10),
    scrollTop: 7,
  });
  h.ttSetNameFilter("anything");
  assert.equal(h.wrap.scrollTop, 7);
});

// ---------- background polling must not fight the user's own scrolling ----------

test("pollTasksTab's regular (non-hash) refresh path does not re-center the selection", () => {
  const fn = boardSource.slice(boardSource.indexOf("async function pollTasksTab()"));
  const body = fn.slice(0, fn.indexOf("\n      }") + "\n      }".length);
  // the hash-restore branch inside the `if` does re-center...
  const ifBlock = body.slice(body.indexOf("if (pendingHash"), body.indexOf("return;\n        }") + "return;\n        }".length);
  assert.match(ifBlock, /ttScrollSelectionIntoView\(\);/);
  // ...but the code after that block (the plain background-poll fallback) does not.
  const afterIf = body.slice(body.indexOf("return;\n        }") + "return;\n        }".length);
  assert.match(afterIf, /renderTasksTab\(\);/);
  assert.doesNotMatch(afterIf, /ttScrollSelectionIntoView/);
});
