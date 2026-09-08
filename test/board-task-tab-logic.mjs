// Loads the Tasks tab's pure decision logic (RAL-362 §8) straight out of
// librarian/assets/board/, so it can't silently drift from what's
// shipped. Same slice-the-real-source approach as ./board-legacy-tasks-hash.mjs
// and ./board-peek-state.mjs -- see those files' headers for why the chunk files
// can't simply be imported as modules.

import { boardScript } from "./board-source.mjs";
import { fileURLToPath } from "node:url";
import { dirname, join } from "node:path";

const REGIONS = [
  ["// RALPHUS-TASK-TAB-COLUMNS:BEGIN", "// RALPHUS-TASK-TAB-COLUMNS:END"],
  ["// RALPHUS-TASK-TAB-LOGIC:BEGIN", "// RALPHUS-TASK-TAB-LOGIC:END"],
];

const repoRoot = join(dirname(fileURLToPath(import.meta.url)), "..");
export const boardPath = join(repoRoot, "librarian", "assets", "board");

const html = boardScript();
const slices = REGIONS.map(([BEGIN, END]) => {
  const from = html.indexOf(BEGIN);
  const to = html.indexOf(END);
  if (from === -1 || to === -1 || to < from) {
    throw new Error(
      `board-task-tab-logic: could not find the ${BEGIN} / ${END} markers in ${boardPath}. ` +
        "If this logic moved, move the markers with it -- these tests are its only coverage.",
    );
  }
  return html.slice(from + BEGIN.length, to);
});
const source = slices.join("\n");

const exported = [
  "TASK_TAB_COLUMNS",
  "TASK_TAB_SORTS",
  "TASK_TAB_DEFAULT_HIDDEN_COLS",
  "taskTabGridTemplate",
  "ttUsageOf",
  "ttCellUsageItems",
  "ttTaskUsageItems",
  "ttTaskUsage",
  "ttCellUsage",
  "ttFmtTokens",
  "ttFmtCache",
  "ttFmtCost",
  "ttTaskReviews",
  "TT_REVIEW_ATTENTION_RANK",
  "ttPickReviewBadge",
  "ttPrsForTask",
  "ttPickTaskPr",
  "TT_PR_COLORS",
  "ttTaskEntityUri",
  "ttSquadEntityUri",
  "ttEffectiveWatch",
  "TT_NEEDS_ME_TIER",
  "ttTierAllows",
  "ttTaskNeedsMe",
  "ttCompareRows",
  "ttRowMatchesFilters",
  "ttGroupAggregate",
  "ttVisibleRange",
  "ttBuildDisplayList",
];
// eslint-disable-next-line no-new-func -- evaluating the real shipped source is the point; see the header.
const factory = new Function(`${source}\nreturn { ${exported.join(", ")} };`);

/** The Tasks tab's pure decision logic, evaluated straight from the board chunks. */
export const taskTabLogic = factory();
