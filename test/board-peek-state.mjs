// Loads the pure peek state machine out of the board chunk files
// (librarian/assets/board/) so it can be exercised under `node --test` with no
// browser and no build step.
//
// The board's JavaScript lives in the librarian/assets/board/*.js chunk files —
// plain scripts sharing one global scope, loaded via board.html's sequential
// <script> tags (and read the same way by eslint.config.mjs / tsconfig.board.json /
// knip.config.js). The chunks are not modules and cannot be imported, so the region
// between the RALPHUS-PEEK-STATE-MACHINE markers — deliberately kept free of DOM,
// fetch and module-level state — is sliced out of their concatenation (this loader
// imports boardScript() from ./board-source.mjs) and evaluated on its own. That
// region is the real shipped source: these tests cannot drift from what the board
// actually runs, because there is only one copy of it.
//
// Everything outside the markers (fetchPeek, peekBox, pollOpenPeeks, ...) needs
// a live document and stays out of scope here by design.

import { boardScript } from "./board-source.mjs";
import { fileURLToPath } from "node:url";
import { dirname, join } from "node:path";

const BEGIN = "// RALPHUS-PEEK-STATE-MACHINE:BEGIN";
const END = "// RALPHUS-PEEK-STATE-MACHINE:END";

const repoRoot = join(dirname(fileURLToPath(import.meta.url)), "..");
export const boardPath = join(repoRoot, "librarian", "assets", "board");

const html = boardScript();
const from = html.indexOf(BEGIN);
const to = html.indexOf(END);
if (from === -1 || to === -1 || to < from) {
  throw new Error(
    `board-peek-state: could not find the ${BEGIN} / ${END} markers in ${boardPath}. ` +
      "If the peek state machine moved, move the markers with it — these tests are its only coverage.",
  );
}
const source = html.slice(from + BEGIN.length, to);

const exported = ["PEEK_MISSING_STRIKE_LIMIT", "peekCssKey", "peekUrlFor", "peekTranscriptUrlFor", "nextPeekPaneState"];
// eslint-disable-next-line no-new-func -- evaluating the real shipped source is the point; see the header.
const factory = new Function(`${source}\nreturn { ${exported.join(", ")} };`);

/** The peek state machine's public surface, evaluated straight from the board chunks. */
export const peek = factory();
