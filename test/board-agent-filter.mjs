// Loads the pure agent-filter logic out of the board chunk files
// (librarian/assets/board/) so it can be exercised under `node --test` with no
// browser and no build step.
//
// The board's JavaScript lives in the librarian/assets/board/*.js chunk files —
// plain scripts sharing one global scope, loaded via board.html's sequential
// <script> tags (and read the same way by eslint.config.mjs / tsconfig.board.json /
// knip.config.js). The chunks are not modules and cannot be imported, so the region
// between the RALPHUS-AGENT-FILTER markers — deliberately kept free of DOM,
// fetch and module-level state — is sliced out of their concatenation (this loader
// imports boardScript() from ./board-source.mjs) and evaluated on its own. That
// region is the real shipped source: these tests cannot drift from what the board
// actually runs, because there is only one copy of it.
//
// Everything outside the markers (agentFilterHtml, renderAgentFilter, the click
// handlers) needs a live document and stays out of scope here by design.

import { boardScript, boardHtmlPath } from "./board-source.mjs";
import { readFileSync } from "node:fs";

const BEGIN = "// RALPHUS-AGENT-FILTER:BEGIN";
const END = "// RALPHUS-AGENT-FILTER:END";

const boardChunks = boardScript();
const from = boardChunks.indexOf(BEGIN);
const to = boardChunks.indexOf(END);
if (from === -1 || to === -1 || to < from) {
  throw new Error(
    `board-agent-filter: could not find the ${BEGIN} / ${END} markers in board chunks. ` +
      "If the agent-filter logic moved, move the markers with it — these tests are its only coverage.",
  );
}
const source = boardChunks.slice(from + BEGIN.length, to);

const exported = [
  "taskAgentNames",
  "squadAgentNames",
  "agentFilterOptions",
  "squadMatchesAgentFilter",
  "agentFilterBadge",
  "syncedAgentSelection",
];
// eslint-disable-next-line no-new-func -- evaluating the real shipped source is the point; see the header.
const factory = new Function(`${source}\nreturn { ${exported.join(", ")} };`);

/** The agent filter's pure decision logic, evaluated straight from the board chunks. */
export const agentFilter = factory();

/** The board HTML and chunks source, for assertions about how the dropdowns are wired up. */
export const boardSource = readFileSync(boardHtmlPath, "utf8") + "\n" + boardChunks;
