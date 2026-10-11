// Loads the Reviews inspector hash encode/decode out of the real board chunks.
import { boardScript } from "./board-source.mjs";

const BEGIN = "// RALPHUS-REVIEW-INSPECTOR-HASH:BEGIN";
const END = "// RALPHUS-REVIEW-INSPECTOR-HASH:END";
const html = boardScript();
const from = html.indexOf(BEGIN);
const to = html.indexOf(END);
if (from === -1 || to === -1 || to < from) {
  throw new Error(`board-review-inspector-hash: could not find the ${BEGIN} / ${END} markers.`);
}
const source = html.slice(from + BEGIN.length, to);
// eslint-disable-next-line no-new-func -- evaluating the real shipped source is the point.
const factory = new Function(`${source}\nreturn { defaultInspectorHashState, encodeInspectorHash, decodeInspectorHash };`);

/** The inspector hash codec, evaluated straight from the board chunks. */
export const inspectorHash = factory();
