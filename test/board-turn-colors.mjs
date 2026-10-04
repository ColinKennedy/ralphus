// Loads the pure per-type turn color logic (RAL-563) out of the board chunks.
import { boardScript } from "./board-source.mjs";

const BEGIN = "// RALPHUS-TURN-COLORS:BEGIN";
const END = "// RALPHUS-TURN-COLORS:END";
const html = boardScript();
const from = html.indexOf(BEGIN);
const to = html.indexOf(END);
if (from === -1 || to === -1 || to < from) {
  throw new Error(`board-turn-colors: could not find the ${BEGIN} / ${END} markers.`);
}
// eslint-disable-next-line no-new-func -- evaluating the real shipped source is the point.
const factory = new Function(`${html.slice(from + BEGIN.length, to)}\nreturn { typeColor, turnContrast, colorizeTurnLines, TURN_COLOR_BG };`);

/** The turn color logic, evaluated straight from the board chunks. */
export const turnColors = factory();
