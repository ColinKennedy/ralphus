// Loads the Logs modal's "output" button logic out of librarian/assets/board.html
// so it can be exercised under `node --test` with no browser and no build step.
//
// Same slice-the-real-source approach as ./board-peek-state.mjs — see that
// file's header for why board.html can't simply be imported. The region
// between the RALPHUS-LOGS-OUTPUT-BTN markers is deliberately free of DOM and
// module-level state; its only free variable is `esc`, which this loader
// injects as a plain pass-through (the real `esc`'s HTML-escaping behaviour
// is out of scope here — these tests are about the button's empty-state
// fallback and key wiring, not HTML escaping).

import { readFileSync } from "node:fs";
import { fileURLToPath } from "node:url";
import { dirname, join } from "node:path";

const BEGIN = "// RALPHUS-LOGS-OUTPUT-BTN:BEGIN";
const END = "// RALPHUS-LOGS-OUTPUT-BTN:END";

const repoRoot = join(dirname(fileURLToPath(import.meta.url)), "..");
export const boardPath = join(repoRoot, "librarian", "assets", "board.html");

const html = readFileSync(boardPath, "utf8");
const from = html.indexOf(BEGIN);
const to = html.indexOf(END);
if (from === -1 || to === -1 || to < from) {
  throw new Error(
    `board-logs-output-btn: could not find the ${BEGIN} / ${END} markers in ${boardPath}. ` +
      "If logsOutputBtn moved, move the markers with it — these tests are its only coverage.",
  );
}
const source = html.slice(from + BEGIN.length, to);

// eslint-disable-next-line no-new-func -- evaluating the real shipped source is the point; see the header.
const factory = new Function("esc", `${source}\nreturn { logsOutputBtn };`);
const passthroughEsc = (s) => String(s ?? "");

/** `logsOutputBtn`, evaluated straight from board.html with a pass-through `esc`. */
export const { logsOutputBtn } = factory(passthroughEsc);

/** The raw board.html source, for assertions about how the button is wired up. */
export const boardSource = html;
