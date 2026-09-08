// Loads the pure "guardian notice -> toast" logic out of the board chunk files (librarian/assets/board/)
// so it can be exercised under `node --test` with no browser and no build step.
//
// Same slice-the-real-source approach as ./board-peek-state.mjs and
// ./board-merge-button.mjs -- see those files' headers for why the chunk
// files can't simply be imported. The region between the RALPHUS-GUARDIAN-NOTICE
// markers is deliberately free of DOM, fetch and module-level state so it
// can be evaluated on its own; `checkGuardianNotices` itself (which mutates
// the shown-map and calls `showInfoToast`) stays out of scope here by
// design.

import { boardScript } from "./board-source.mjs";
import { fileURLToPath } from "node:url";
import { dirname, join } from "node:path";

const BEGIN = "// RALPHUS-GUARDIAN-NOTICE:BEGIN";
const END = "// RALPHUS-GUARDIAN-NOTICE:END";

const repoRoot = join(dirname(fileURLToPath(import.meta.url)), "..");
export const boardPath = join(repoRoot, "librarian", "assets", "board");

const html = boardScript();
const from = html.indexOf(BEGIN);
const to = html.indexOf(END);
if (from === -1 || to === -1 || to < from) {
  throw new Error(
    `board-guardian-notice: could not find the ${BEGIN} / ${END} markers in ${boardPath}. ` +
      "If the guardian-notice logic moved, move the markers with it — these tests are its only coverage.",
  );
}
const source = html.slice(from + BEGIN.length, to);

const exported = ["pendingGuardianNoticeToasts"];
// eslint-disable-next-line no-new-func -- evaluating the real shipped source is the point; see the header.
const factory = new Function(`${source}\nreturn { ${exported.join(", ")} };`);

/** The guardian-notice view logic, evaluated straight from the board chunks. */
export const guardianNotice = factory();

/** The raw board script (all chunks concatenated), for assertions about how the toast is wired up. */
export const boardSource = html;
