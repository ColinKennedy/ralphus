// Loads the pure token/cost-summary helpers out of librarian/assets/board.html
// so they can be exercised under `node --test` with no browser and no build step.
//
// Same slice-the-real-source approach as ./board-merge-button.mjs — see that
// file's header for why board.html can't simply be imported. The region
// between the RALPHUS-USAGE-SUMMARY markers is deliberately free of DOM,
// fetch and module-level state so it can be evaluated on its own.

import { readFileSync } from "node:fs";
import { fileURLToPath } from "node:url";
import { dirname, join } from "node:path";

const BEGIN = "// RALPHUS-USAGE-SUMMARY:BEGIN";
const END = "// RALPHUS-USAGE-SUMMARY:END";

const repoRoot = join(dirname(fileURLToPath(import.meta.url)), "..");
export const boardPath = join(repoRoot, "librarian", "assets", "board.html");

const html = readFileSync(boardPath, "utf8");
const from = html.indexOf(BEGIN);
const to = html.indexOf(END);
if (from === -1 || to === -1 || to < from) {
  throw new Error(
    `board-usage-summary: could not find the ${BEGIN} / ${END} markers in ${boardPath}. ` +
      "If the usage-summary view logic moved, move the markers with it — these tests are its only coverage.",
  );
}
const source = html.slice(from + BEGIN.length, to);

const exported = [
  "fmtCostUsd",
  "usageSummary",
  "uncachedInputRateUsd",
  "compactionCostUsd",
  "compactionSummary",
  "estimatedBadge",
];
// eslint-disable-next-line no-new-func -- evaluating the real shipped source is the point; see the header.
const factory = new Function(`${source}\nreturn { ${exported.join(", ")} };`);

/** The usage-summary helpers, evaluated straight from board.html. */
export const usageSummaryHelpers = factory();

/** The raw board.html source, for assertions about how the helpers are wired up. */
export const boardSource = html;
