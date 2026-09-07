// Loads the pure "is this #tasks... hash actually an old squad-viewer link"
// check out of librarian/assets/board.html, so RAL-362's legacy-redirect
// behavior (old `#tasks...` links must still land on the Squads tab) cannot
// silently drift from what's shipped.
//
// Same slice-the-real-source approach as ./board-peek-state.mjs and friends
// -- see those files' headers for why board.html can't simply be imported.
// `isLegacySquadsTasksHash` depends on two small helper regions
// (`splitHashSel`, and the `looksLikeUri`/`parseRalphusUri` URI codec) that
// live elsewhere in the file and are not themselves contiguous with it, so
// this loader concatenates all three marker regions before evaluating --
// each one is independently free of DOM, fetch and module-level state.

import { readFileSync } from "node:fs";
import { fileURLToPath } from "node:url";
import { dirname, join } from "node:path";

const REGIONS = [
  ["// RALPHUS-URI-CODEC:BEGIN", "// RALPHUS-URI-CODEC:END"],
  ["// RALPHUS-HASH-SEL:BEGIN", "// RALPHUS-HASH-SEL:END"],
  ["// RALPHUS-LEGACY-TASKS-HASH:BEGIN", "// RALPHUS-LEGACY-TASKS-HASH:END"],
];

const repoRoot = join(dirname(fileURLToPath(import.meta.url)), "..");
export const boardPath = join(repoRoot, "librarian", "assets", "board.html");

const html = readFileSync(boardPath, "utf8");
const slices = REGIONS.map(([BEGIN, END]) => {
  const from = html.indexOf(BEGIN);
  const to = html.indexOf(END);
  if (from === -1 || to === -1 || to < from) {
    throw new Error(
      `board-legacy-tasks-hash: could not find the ${BEGIN} / ${END} markers in ${boardPath}. ` +
        "If this logic moved, move the markers with it -- these tests are its only coverage.",
    );
  }
  return html.slice(from + BEGIN.length, to);
});
const source = slices.join("\n");

const exported = ["isLegacySquadsTasksHash", "splitHashSel", "looksLikeUri", "parseRalphusUri"];
// eslint-disable-next-line no-new-func -- evaluating the real shipped source is the point; see the header.
const factory = new Function(`${source}\nreturn { ${exported.join(", ")} };`);

/** The legacy-tasks-hash detection logic, evaluated straight from board.html. */
export const legacyTasksHash = factory();

/** The raw board.html source, for assertions about how `parseHash` dispatches on it. */
export const boardSource = html;
