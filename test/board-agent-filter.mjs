// Loads the pure agent-filter logic out of librarian/assets/board.html so it
// can be exercised under `node --test` with no browser and no build step.
//
// Same slice-the-real-source approach as ./board-peek-state.mjs — see that
// file's header for why board.html can't simply be imported. The region
// between the RALPHUS-AGENT-FILTER markers is deliberately free of DOM, fetch
// and module-level state so it can be evaluated on its own; everything that
// needs a live document (agentFilterHtml, renderAgentFilter, the click
// handlers) stays out of scope here by design and calls into these.

import { readFileSync } from "node:fs";
import { fileURLToPath } from "node:url";
import { dirname, join } from "node:path";

const BEGIN = "// RALPHUS-AGENT-FILTER:BEGIN";
const END = "// RALPHUS-AGENT-FILTER:END";

const repoRoot = join(dirname(fileURLToPath(import.meta.url)), "..");
export const boardPath = join(repoRoot, "librarian", "assets", "board.html");

const html = readFileSync(boardPath, "utf8");
const from = html.indexOf(BEGIN);
const to = html.indexOf(END);
if (from === -1 || to === -1 || to < from) {
  throw new Error(
    `board-agent-filter: could not find the ${BEGIN} / ${END} markers in ${boardPath}. ` +
      "If the agent-filter logic moved, move the markers with it — these tests are its only coverage.",
  );
}
const source = html.slice(from + BEGIN.length, to);

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

/** The agent filter's pure decision logic, evaluated straight from board.html. */
export const agentFilter = factory();

/** The raw board.html source, for assertions about how the dropdowns are wired up. */
export const boardSource = html;
