// Loads the pure transcript-tape line pipeline out of the board chunk files
// (librarian/assets/board/) so it can be exercised under `node --test` with no
// browser and no build step (RAL-397 Phase 2G-A).
//
// The pipeline (ANSI-strip + classify each complete line, honoring the Debug
// toggle) is kept free of DOM/fetch/module-level state between the
// RALPHUS-TAPE-LINES markers so it can be sliced out of the chunks'
// concatenation and evaluated standalone — the same pattern
// board-peek-state.mjs uses. The tests read the real shipped source.

import { boardScript } from "./board-source.mjs";
import { fileURLToPath } from "node:url";
import { dirname, join } from "node:path";

const BEGIN = "// RALPHUS-TAPE-LINES:BEGIN";
const END = "// RALPHUS-TAPE-LINES:END";

const repoRoot = join(dirname(fileURLToPath(import.meta.url)), "..");
export const boardPath = join(repoRoot, "librarian", "assets", "board");

const html = boardScript();
const from = html.indexOf(BEGIN);
const to = html.indexOf(END);
if (from === -1 || to === -1 || to < from) {
  throw new Error(
    `board-tape-lines: could not find the ${BEGIN} / ${END} markers in ${boardPath}. ` +
      "If the tape line pipeline moved, move the markers with it — these tests are its only coverage.",
  );
}
const source = html.slice(from + BEGIN.length, to);

const exported = ["stripAnsiEscapes", "formatInlineTapeEvent", "classifyTapeLine", "renderTapeLines"];
// eslint-disable-next-line no-new-func -- evaluating the real shipped source is the point; see the header.
const factory = new Function(`${source}\nreturn { ${exported.join(", ")} };`);

/** The tape line pipeline's public surface, evaluated straight from the board chunks. */
export const lines = factory();
