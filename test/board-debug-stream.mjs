// Loads the pure unified debug/terminal-log stream logic (RAL-296) out of
// librarian/assets/board/ so it can be exercised under `node --test`
// with no browser and no build step.
//
// Same slice-the-real-source approach as ./board-peek-state.mjs and
// ./board-merge-button.mjs -- see board-peek-state.mjs's header for why
// the chunk files can't simply be imported. The region between the
// RALPHUS-DEBUG-STREAM markers is deliberately free of DOM, fetch and
// module-level state so it can be evaluated on its own; everything that
// needs a live document or module state (currentPeekDisplayText,
// viewHistoryAttempt, the peekDebugEvents/historyAttempts caches) stays out
// of scope here by design.

import { boardScript } from "./board-source.mjs";
import { fileURLToPath } from "node:url";
import { dirname, join } from "node:path";

const BEGIN = "// RALPHUS-DEBUG-STREAM:BEGIN";
const END = "// RALPHUS-DEBUG-STREAM:END";

const repoRoot = join(dirname(fileURLToPath(import.meta.url)), "..");
export const boardPath = join(repoRoot, "librarian", "assets", "board");

const html = boardScript();
const from = html.indexOf(BEGIN);
const to = html.indexOf(END);
if (from === -1 || to === -1 || to < from) {
  throw new Error(
    `board-debug-stream: could not find the ${BEGIN} / ${END} markers in ${boardPath}. ` +
      "If the debug-stream logic moved, move the markers with it — these tests are its only coverage.",
  );
}
const source = html.slice(from + BEGIN.length, to);

const exported = ["debugEventsUrlFor", "formatDebugEvent", "isMostRecentAttempt"];
// eslint-disable-next-line no-new-func -- evaluating the real shipped source is the point; see the header.
const factory = new Function(`${source}\nreturn { ${exported.join(", ")} };`);

/** The unified debug/terminal-log stream's pure logic, evaluated straight from the board chunks. */
export const debugStream = factory();

/** The raw board script (all chunks concatenated), for assertions about how the stream is wired up. */
export const boardSource = html;
