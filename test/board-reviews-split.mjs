// Loads the RAL-417 Reviews-sidebar splitter logic out of the board chunk files
// (librarian/assets/board/) so it can be exercised under `node --test` with no
// browser and no build step.
//
// Same slice-the-real-source approach as ./board-peek-state.mjs -- the region
// between the RALPHUS-REVIEWS-SPLIT markers is the shipped code itself, so the
// tests can't silently drift from what the librarian serves. The marker region
// holds the pure max-width math (viewport width in, sidebar ceiling out) plus
// its window-thin wrapper; everything that touches the live document
// (initSplitters, onSplitMove/onSplitUp, applyPaneWidths, ...) stays out of
// scope here and is pinned by source-shape wiring assertions instead (see
// ./reviews-split.test.mjs).

import { boardScript } from "./board-source.mjs";

const BEGIN = "// RALPHUS-REVIEWS-SPLIT:BEGIN";
const END = "// RALPHUS-REVIEWS-SPLIT:END";

const html = boardScript();
const from = html.indexOf(BEGIN);
const to = html.indexOf(END);
if (from === -1 || to === -1 || to < from) {
  throw new Error(
    `board-reviews-split: could not find the ${BEGIN} / ${END} markers in the board chunks. ` +
      "If the Reviews sidebar max-width logic moved, move the markers with it -- these tests are its only coverage.",
  );
}
const source = html.slice(from + BEGIN.length, to);

/**
 * Evaluates the Reviews-sidebar max-width logic with the real shipped
 * SPLIT_CFG `min` (regex-extracted from the chunk, so a changed floor flows
 * into these tests automatically) and a stub `window` width.
 * @param {{innerWidth?: number}} [opts]
 * @returns {{MIN_REVIEW_DETAIL_W: number, reviewsSidebarMaxWFor: (viewportW: number) => number, reviewsSidebarMaxW: () => number}}
 */
export function loadReviewsSplit({ innerWidth = 1440 } = {}) {
  const m = html.match(/"--reviews-sidebar-w": \{\s*key: "[^"]+",\s*def: \d+,\s*min: (\d+)\s*\}/);
  if (!m) {
    throw new Error('board-reviews-split: could not parse the SPLIT_CFG "--reviews-sidebar-w" entry from the shipped chunk');
  }
  const splitCfg = { "--reviews-sidebar-w": { min: Number(m[1]) } };
  // eslint-disable-next-line no-new-func -- evaluating the real shipped source is the point; see the header.
  const factory = new Function(
    `const SPLIT_CFG = ${JSON.stringify(splitCfg)};
     const window = { innerWidth: ${innerWidth} };
     ${source}
     return { MIN_REVIEW_DETAIL_W, reviewsSidebarMaxWFor, reviewsSidebarMaxW };`,
  );
  return factory();
}