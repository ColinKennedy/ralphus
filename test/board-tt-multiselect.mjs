// Loads the RAL-350 Tasks-tab multi-selection logic out of the board chunk
// files (librarian/assets/board/) so it can be exercised under `node --test`
// with no browser and no build step.
//
// Same slice-the-real-source approach as ./board-filter-selection-scroll.mjs
// and ./board-task-tab-logic.mjs -- the region is the shipped code itself, so
// the tests can't silently drift from what the librarian serves.

import { boardScript } from "./board-source.mjs";
import { fileURLToPath } from "node:url";
import { dirname, join } from "node:path";

const repoRoot = join(dirname(fileURLToPath(import.meta.url)), "..");
export const boardPath = join(repoRoot, "librarian", "assets", "board");

const html = boardScript();

/** Slices one marker-delimited region out of the real board script. */
function sliceRegion([BEGIN, END]) {
  const from = html.indexOf(BEGIN);
  const to = html.indexOf(END);
  if (from === -1 || to === -1 || to < from) {
    throw new Error(
      `board-tt-multiselect: could not find the ${BEGIN} / ${END} markers in ${boardPath}. ` +
        "If this logic moved, move the markers with it -- these tests are its only coverage.",
    );
  }
  return html.slice(from + BEGIN.length, to);
}

const REGION = ["// RALPHUS-TT-MULTISELECT:BEGIN", "// RALPHUS-TT-MULTISELECT:END"];

/**
 * Builds the sandboxed Tasks-tab multi-selection helpers (RAL-350:
 * `ttVisibleTaskKeys`, `ttSelectAllState`, `ttToggleSelectAllVisible`,
 * `ttToggleRowSel`, `ttVisibleSelectedRows`) with injectable state.
 * `renderTasksTab` is a call-counting stub only -- the real function's own
 * rendering is covered elsewhere; this harness is purely about the
 * selection-set bookkeeping.
 */
export function makeMultiSelect({ ttDisplayItems = [], ttAllRows = [], ttSel = new Set(), ttSelAnchor = null } = {}) {
  const calls = { renderTasksTab: 0 };
  const deps = {
    renderTasksTab: () => { calls.renderTasksTab++; },
    ttDisplayItems,
    ttAllRows,
    ttSel,
    ttSelAnchor,
  };
  // eslint-disable-next-line no-new-func -- evaluating the real shipped source is the point; see the header.
  const factory = new Function(
    "deps",
    `const { renderTasksTab } = deps;
     var ttDisplayItems = deps.ttDisplayItems;
     var ttAllRows = deps.ttAllRows;
     var ttSel = deps.ttSel;
     var ttSelAnchor = deps.ttSelAnchor;
     ${sliceRegion(REGION)}
     return {
       ttVisibleTaskKeys, ttSelectAllState, ttToggleSelectAllVisible, ttToggleRowSel, ttVisibleSelectedRows,
       state: () => ({ ttSel, ttSelAnchor }),
     };`,
  );
  const api = factory(deps);
  return { ...api, calls };
}
