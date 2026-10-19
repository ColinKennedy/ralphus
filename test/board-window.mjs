// Loads the board's squad-window logic (STRESS-004) out of the board chunk
// files so it can be exercised under `node --test`. The region between the
// RALPHUS-BOARD-WINDOW markers is the real shipped source; see
// board-peek-state.mjs for why the chunks are sliced rather than imported.

import { boardScript } from "./board-source.mjs";

const BEGIN = "// RALPHUS-BOARD-WINDOW:BEGIN";
const END = "// RALPHUS-BOARD-WINDOW:END";

const script = boardScript();
const from = script.indexOf(BEGIN);
const to = script.indexOf(END);
if (from === -1 || to === -1 || to < from) {
  throw new Error(
    `board-window: could not find the ${BEGIN} / ${END} markers in the board chunks. ` +
      "If the window logic moved, move the markers with it — these tests are its only coverage.",
  );
}
const source = script.slice(from + BEGIN.length, to);

/** A fresh copy of the window logic and its state, per test. */
export function makeWindow() {
  // eslint-disable-next-line no-new-func -- evaluating the real shipped source is the point; see the header.
  const factory = new Function(
    `${source}\nreturn { BOARD_WINDOW, boardWindow, boardWindowUrl, noteBoardWindow, mergeWindowedSquads, windowedSquads, boardWindowFooter, boardWindowSearchText, boardSearchUrl, squadMatchesSearch, unionById };`,
  );
  return factory();
}
