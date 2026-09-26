// Loads the Live View "Show Thinking" checkbox's per-pane precedence logic
// (RAL-516) -- peekShowsThinking/toggleShowThinking -- straight out of
// librarian/assets/board/, sliced by the RALPHUS-SHOW-THINKING markers.
// Same slice-the-real-source approach as ./board-project-filter-menu.mjs --
// this logic closes over module-level state (peekShowThinking,
// hideThinkingDefault, peekTape, renderPeekTape), so it needs injectable
// stand-ins for that state rather than being evaluable standalone the way
// ./board-debug-stream.mjs's fully pure region is.

import { boardScript } from "./board-source.mjs";
import { fileURLToPath } from "node:url";
import { dirname, join } from "node:path";

const BEGIN = "// RALPHUS-SHOW-THINKING:BEGIN";
const END = "// RALPHUS-SHOW-THINKING:END";

const repoRoot = join(dirname(fileURLToPath(import.meta.url)), "..");
export const boardPath = join(repoRoot, "librarian", "assets", "board");

const html = boardScript();
const from = html.indexOf(BEGIN);
const to = html.indexOf(END);
if (from === -1 || to === -1 || to < from) {
  throw new Error(
    `board-thinking-precedence: could not find the ${BEGIN} / ${END} markers in ${boardPath}. ` +
      "If this logic moved, move the markers with it -- these tests are its only coverage.",
  );
}
const source = html.slice(from + BEGIN.length, to);

/**
 * Builds peekShowsThinking/toggleShowThinking with injectable stand-ins for
 * the module-level state they close over: `peekShowThinking` (the per-pane
 * override map), `hideThinkingDefault` (the config-driven fallback), and
 * `peekTape`/`renderPeekTape` (the already-loaded tape window they re-render
 * on toggle).
 */
export function makeThinkingPrecedence({
  peekShowThinking = {},
  hideThinkingDefault = false,
  peekTape = {},
} = {}) {
  const calls = { renderPeekTape: [] };
  const deps = {
    peekShowThinking,
    hideThinkingDefault,
    peekTape,
    renderPeekTape: (key) => calls.renderPeekTape.push(key),
  };
  // eslint-disable-next-line no-new-func -- evaluating the real shipped source is the point; see the header.
  const factory = new Function(
    "deps",
    `var peekShowThinking = deps.peekShowThinking;
     var hideThinkingDefault = deps.hideThinkingDefault;
     var peekTape = deps.peekTape;
     var renderPeekTape = deps.renderPeekTape;
     ${source}
     return { peekShowsThinking, toggleShowThinking };`,
  );
  const api = factory(deps);
  return { ...api, calls, peekShowThinking, peekTape };
}
