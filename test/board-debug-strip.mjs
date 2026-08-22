// Loads the pure Live View debug-line-stripping logic out of
// librarian/assets/board.html so it can be exercised under `node --test` with
// no browser and no build step.
//
// board.html is a single static page whose entire client is one inline
// <script> (see eslint.config.mjs / tsconfig.board.json / knip.config.js for
// the three lint layers that read it the same way). It is not a module and
// cannot be imported, so the region between the RALPHUS-DEBUG-STRIP markers
// — deliberately kept free of DOM, fetch and module-level state — is sliced
// out and evaluated on its own, mirroring board-peek-state.mjs's approach to
// the peek state machine. That region is the real shipped source: these
// tests cannot drift from what the board actually runs, because there is
// only one copy of it.

import { readFileSync } from "node:fs";
import { fileURLToPath } from "node:url";
import { dirname, join } from "node:path";

const BEGIN = "// RALPHUS-DEBUG-STRIP:BEGIN";
const END = "// RALPHUS-DEBUG-STRIP:END";

const repoRoot = join(dirname(fileURLToPath(import.meta.url)), "..");
export const boardPath = join(repoRoot, "librarian", "assets", "board.html");

const html = readFileSync(boardPath, "utf8");
const from = html.indexOf(BEGIN);
const to = html.indexOf(END);
if (from === -1 || to === -1 || to < from) {
  throw new Error(
    `board-debug-strip: could not find the ${BEGIN} / ${END} markers in ${boardPath}. ` +
      "If the debug-line-stripping logic moved, move the markers with it — these tests are its only coverage.",
  );
}
const source = html.slice(from + BEGIN.length, to);

const exported = ["isRalphusDebugLine", "stripDebugLines"];
// eslint-disable-next-line no-new-func -- evaluating the real shipped source is the point; see the header.
const factory = new Function(`${source}\nreturn { ${exported.join(", ")} };`);

/** The debug-line-stripping logic's public surface, evaluated straight from board.html. */
export const debugStrip = factory();
