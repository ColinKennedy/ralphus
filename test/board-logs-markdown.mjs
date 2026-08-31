// Loads the pure Markdown-table serialization helpers backing the Logs
// modal's "copy as Markdown" buttons (RAL-294) out of librarian/assets/board.html
// so they can be exercised under `node --test` with no browser and no build step.
//
// board.html is a single static page whose entire client is one inline
// <script> (see eslint.config.mjs / tsconfig.board.json / knip.config.js for
// the three lint layers that read it the same way). It is not a module and
// cannot be imported, so the region between the RALPHUS-LOGS-MARKDOWN markers
// — deliberately kept free of DOM, fetch and module-level state — is sliced
// out and evaluated on its own. That region is the real shipped source: these
// tests cannot drift from what the board actually runs, because there is only
// one copy of it.
//
// The marked region includes all four tab builders. Their few page-level
// dependencies are injected by createLogsMd(), while the copy-button/menu DOM
// wiring stays outside the region.

import { readFileSync } from "node:fs";
import { fileURLToPath } from "node:url";
import { dirname, join } from "node:path";

const BEGIN = "// RALPHUS-LOGS-MARKDOWN:BEGIN";
const END = "// RALPHUS-LOGS-MARKDOWN:END";

const repoRoot = join(dirname(fileURLToPath(import.meta.url)), "..");
export const boardPath = join(repoRoot, "librarian", "assets", "board.html");

const html = readFileSync(boardPath, "utf8");
const from = html.indexOf(BEGIN);
const to = html.indexOf(END);
if (from === -1 || to === -1 || to < from) {
  throw new Error(
    `board-logs-markdown: could not find the ${BEGIN} / ${END} markers in ${boardPath}. ` +
      "If the Markdown serialization helpers moved, move the markers with them — these tests are their only coverage.",
  );
}
const source = html.slice(from + BEGIN.length, to);

const exported = [
  "mdEscapeCell",
  "mdCellWithFootnote",
  "mdHeadCell",
  "mdTableToText",
  "eventsMdTable",
  "tasksMdTable",
  "cellsMdTable",
  "proofsMdTable",
  "buildLogsMdTable",
];
// eslint-disable-next-line no-new-func -- evaluating the real shipped source is the point; see the header.
const factory = new Function(
  "TASK_REASON_TOOLTIP",
  "CELL_CUM_TOOLTIP",
  "cartoModalCache",
  "cartoFetch",
  "cartoResolveEntity",
  "fmtCostUsd",
  `${source}\nreturn { ${exported.join(", ")} };`,
);

/** The Logs-modal Markdown serialization helpers, evaluated straight from board.html. */
export function createLogsMd({
  cartoModalCache = {},
  cartoFetch = async () => ({ rows: [], total: 0 }),
  cartoResolveEntity = () => null,
  fmtCostUsd = (value) => `$${Number(value).toFixed(4)}`,
} = {}) {
  return factory(
    "Why a task itself failed when no cell or proof error explains it.",
    "Cumulative usage across every completed attempt, including restarts.",
    cartoModalCache,
    cartoFetch,
    cartoResolveEntity,
    fmtCostUsd,
  );
}

export const logsMd = createLogsMd();
