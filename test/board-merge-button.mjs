// Loads the pure "Merge / rebase" button logic out of librarian/assets/board.html
// so it can be exercised under `node --test` with no browser and no build step.
//
// Same slice-the-real-source approach as ./board-peek-state.mjs — see that
// file's header for why board.html can't simply be imported. The region
// between the RALPHUS-MERGE-BUTTON markers is deliberately free of DOM,
// fetch and module-level state so it can be evaluated on its own; everything
// that needs a live document (renderReviewDetail, mergeReview, the toast
// helpers) stays out of scope here by design.

import { readFileSync } from "node:fs";
import { fileURLToPath } from "node:url";
import { dirname, join } from "node:path";

const BEGIN = "// RALPHUS-MERGE-BUTTON:BEGIN";
const END = "// RALPHUS-MERGE-BUTTON:END";

const repoRoot = join(dirname(fileURLToPath(import.meta.url)), "..");
export const boardPath = join(repoRoot, "librarian", "assets", "board.html");

const html = readFileSync(boardPath, "utf8");
const from = html.indexOf(BEGIN);
const to = html.indexOf(END);
if (from === -1 || to === -1 || to < from) {
  throw new Error(
    `board-merge-button: could not find the ${BEGIN} / ${END} markers in ${boardPath}. ` +
      "If the merge-button view logic moved, move the markers with it — these tests are its only coverage.",
  );
}
const source = html.slice(from + BEGIN.length, to);

const exported = ["MERGE_STARTABLE", "mergeButtonView", "mergeRequestedToast"];
// eslint-disable-next-line no-new-func -- evaluating the real shipped source is the point; see the header.
const factory = new Function(`${source}\nreturn { ${exported.join(", ")} };`);

/** The merge button's view logic, evaluated straight from board.html. */
export const mergeButton = factory();

/** The raw board.html source, for assertions about how the button is wired up. */
export const boardSource = html;
