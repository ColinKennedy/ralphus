// Loads the RAL-419 squad-selection transition/reconciliation logic out of the
// board chunk files (librarian/assets/board/) so it can be exercised under
// `node --test` with no browser and no build step.
//
// The board's JavaScript lives in the librarian/assets/board/*.js chunk files —
// plain scripts sharing one global scope, loaded via board.html's sequential
// <script> tags (and read the same way by eslint.config.mjs / tsconfig.board.json /
// knip.config.js). The region between the RALPHUS-SEL-TRANSITION markers — kept
// deliberately free of DOM, fetch, localStorage and module-level state — is
// sliced out of their concatenation (via boardScript() from ./board-source.mjs)
// and evaluated on its own: state in, state out. That region is the real shipped
// source: these tests cannot drift from what the board actually runs, because
// there is only one copy of it.

import { boardScript } from "./board-source.mjs";
import { fileURLToPath } from "node:url";
import { dirname, join } from "node:path";

const BEGIN = "// RALPHUS-SEL-TRANSITION:BEGIN";
const END = "// RALPHUS-SEL-TRANSITION:END";

const repoRoot = join(dirname(fileURLToPath(import.meta.url)), "..");
export const boardPath = join(repoRoot, "librarian", "assets", "board");

const html = boardScript();
const from = html.indexOf(BEGIN);
const to = html.indexOf(END);
if (from === -1 || to === -1 || to < from) {
  throw new Error(
    `board-sel-transition: could not find the ${BEGIN} / ${END} markers in ${boardPath}. ` +
      "If the selection-transition region moved, move the markers with it — these tests are its only coverage.",
  );
}
const source = html.slice(from + BEGIN.length, to);

const exported = ["squadLevelSel", "snapshotSel", "reconcileSquadSelection", "nodeKeyValid", "transitionSquadSelection", "pruneSquadSelCache"];
// eslint-disable-next-line no-new-func -- evaluating the real shipped source is the point; see the header.
const factory = new Function(`${source}\nreturn { ${exported.join(", ")} };`);

/** The selection-transition machine's public surface, evaluated straight from the board chunks. */
export const selTransition = factory();