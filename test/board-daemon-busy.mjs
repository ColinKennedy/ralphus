// Loads the board's pure `503` backoff (STRESS-006) out of the board chunk
// files so it can be exercised under `node --test`. The region between the
// RALPHUS-DAEMON-BUSY markers is the real shipped source; see
// board-peek-state.mjs for why the chunks are sliced rather than imported.

import { boardScript } from "./board-source.mjs";

const BEGIN = "// RALPHUS-DAEMON-BUSY:BEGIN";
const END = "// RALPHUS-DAEMON-BUSY:END";

const script = boardScript();
const from = script.indexOf(BEGIN);
const to = script.indexOf(END);
if (from === -1 || to === -1 || to < from) {
  throw new Error(
    `board-daemon-busy: could not find the ${BEGIN} / ${END} markers in the board chunks. ` +
      "If the backoff moved, move the markers with it — these tests are its only coverage.",
  );
}
const source = script.slice(from + BEGIN.length, to);

// eslint-disable-next-line no-new-func -- evaluating the real shipped source is the point; see the header.
const factory = new Function(`${source}\nreturn { daemonBusyDelayMs };`);

/** The backoff's public surface, evaluated straight from the board chunks. */
export const busy = factory();
