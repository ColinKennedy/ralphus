// Loads the pure Live View type-filter logic directly from shipped board code.
import { boardScript } from "./board-source.mjs";

const BEGIN = "// RALPHUS-LIVE-VIEW-TYPES:BEGIN";
const END = "// RALPHUS-LIVE-VIEW-TYPES:END";
const html = boardScript();
const from = html.indexOf(BEGIN);
const to = html.indexOf(END);
if (from === -1 || to === -1 || to < from) {
  throw new Error(`board-live-view-types: could not find the ${BEGIN} / ${END} markers.`);
}
// eslint-disable-next-line no-new-func -- evaluating the real shipped source is the point.
const factory = new Function(`${html.slice(from + BEGIN.length, to)}\nreturn { parseLiveViewType, filterLiveViewTypes };`);

/** The Live View type-filter logic, evaluated straight from the board chunks. */
export const liveViewTypes = factory();
