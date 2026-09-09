// Loads the RAL-383 "preserve selection after clearing filters" logic out of
// the board chunk files (librarian/assets/board/) so it can be exercised
// under `node --test` with no browser and no build step.
//
// Same slice-the-real-source approach as ./board-review-badge-nav.mjs and
// ./board-task-tab-logic.mjs -- the regions are the shipped code itself, so
// the tests can't silently drift from what the librarian serves.

import { boardScript } from "./board-source.mjs";
import { fileURLToPath } from "node:url";
import { dirname, join } from "node:path";

const repoRoot = join(dirname(fileURLToPath(import.meta.url)), "..");
export const boardPath = join(repoRoot, "librarian", "assets", "board");

const html = boardScript();

/** The raw board script (all chunks concatenated), for source-level wiring assertions. */
export const boardSource = html;

/** Slices one marker-delimited region out of the real board script. */
function sliceRegion([BEGIN, END]) {
  const from = html.indexOf(BEGIN);
  const to = html.indexOf(END);
  if (from === -1 || to === -1 || to < from) {
    throw new Error(
      `board-filter-selection-scroll: could not find the ${BEGIN} / ${END} markers in ${boardPath}. ` +
        "If this logic moved, move the markers with it -- these tests are its only coverage.",
    );
  }
  return html.slice(from + BEGIN.length, to);
}

const REGIONS = {
  scroll: ["// RALPHUS-TT-SCROLL-SELECTION:BEGIN", "// RALPHUS-TT-SCROLL-SELECTION:END"],
  filters: ["// RALPHUS-TT-FILTER-SELECTION-SCROLL:BEGIN", "// RALPHUS-TT-FILTER-SELECTION-SCROLL:END"],
};

/**
 * Builds the sandboxed Tasks-tab filter setters (`ttSetNameFilter`,
 * `ttToggleShowHidden`, `ttToggleNeedsMe`, `ttToggleStatus`, `ttAllStatus`)
 * plus `ttScrollSelectionIntoView` with injectable DOM/collaborator stubs.
 * `renderTasksTab` is a call-counting stub only -- the real function's own
 * filtering is already covered by board-task-tab-logic.test.mjs; this harness
 * is purely about the wiring (does a filter change re-center the retained
 * selection?) and the scroll-offset math.
 */
export function makeFilterScroll({
  taskTabFilters = { q: "", status: new Set(["running"]), showHidden: false, needsMe: false, groupBySquad: false, sort: "name", dir: 1 },
  taskTabSel = { kind: "task", squadId: "sq-1", taskIdx: 5, cellIdx: -1 },
  ttDisplayItems = [],
  scrollTop = 0,
  clientHeight = 300,
  headHeight = 32,
  rowH = 32,
} = {}) {
  const calls = { renderTasksTab: 0, syncHash: 0, renderTtStatusFilters: 0, ttRenderVisibleRows: 0, order: [] };
  const wrap = { scrollTop, clientHeight };
  const head = { offsetHeight: headHeight };
  const elements = { "tt-table-wrap": wrap, "tt-head": head };
  const deps = {
    renderTasksTab: () => { calls.renderTasksTab++; calls.order.push("renderTasksTab"); },
    syncHash: () => { calls.syncHash++; calls.order.push("syncHash"); },
    renderTtStatusFilters: () => { calls.renderTtStatusFilters++; calls.order.push("renderTtStatusFilters"); },
    ttRenderVisibleRows: () => { calls.ttRenderVisibleRows++; calls.order.push("ttRenderVisibleRows"); },
    byId: (id) => elements[id],
    TT_ROW_H: rowH,
    STATES: ["queued", "running", "done", "failed", "ignored"],
    taskTabFilters,
    taskTabSel,
    ttDisplayItems,
  };
  // eslint-disable-next-line no-new-func -- evaluating the real shipped source is the point; see the header.
  const factory = new Function(
    "deps",
    `const { renderTasksTab, syncHash, renderTtStatusFilters, ttRenderVisibleRows, byId, TT_ROW_H, STATES } = deps;
     var taskTabFilters = deps.taskTabFilters;
     var taskTabSel = deps.taskTabSel;
     var ttDisplayItems = deps.ttDisplayItems;
     ${sliceRegion(REGIONS.scroll)}
     ${sliceRegion(REGIONS.filters)}
     return {
       ttSetNameFilter, ttToggleShowHidden, ttToggleNeedsMe, ttToggleStatus, ttAllStatus, ttScrollSelectionIntoView,
       state: () => ({ taskTabFilters, taskTabSel, ttDisplayItems }),
     };`,
  );
  const api = factory(deps);
  return { ...api, calls, wrap, head };
}
