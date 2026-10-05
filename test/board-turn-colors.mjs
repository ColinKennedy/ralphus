// Loads the pure per-type turn color logic (RAL-563) out of the board chunks.
import { boardScript } from "./board-source.mjs";

const BEGIN = "// RALPHUS-TURN-COLORS:BEGIN";
const END = "// RALPHUS-TURN-COLORS:END";
const TYPE_BEGIN = "// RALPHUS-LIVE-VIEW-TYPES:BEGIN";
const TYPE_END = "// RALPHUS-LIVE-VIEW-TYPES:END";
const html = boardScript();
const from = html.indexOf(BEGIN);
const to = html.indexOf(END);
const typeFrom = html.indexOf(TYPE_BEGIN);
const typeTo = html.indexOf(TYPE_END);
if (from === -1 || to === -1 || to < from || typeFrom === -1 || typeTo === -1 || typeTo < typeFrom) {
  throw new Error(`board-turn-colors: could not find the shared type or ${BEGIN} / ${END} markers.`);
}
// eslint-disable-next-line no-new-func -- evaluating the real shipped source is the point.
const factory = new Function(`${html.slice(typeFrom + TYPE_BEGIN.length, typeTo)}\n${html.slice(from + BEGIN.length, to)}\nreturn { typeColor, turnContrast, colorizeTurnLines, TURN_COLOR_BG };`);

/** The turn color logic, evaluated straight from the board chunks. */
export const turnColors = factory();
