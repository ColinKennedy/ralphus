// Loads the RAL-537 auto-fix/base-shift exhaustion indicators out of the
// board chunk files (librarian/assets/board/) so they can be exercised under
// `node --test` with no browser and no build step.
//
// Same slice-the-real-source approach as ./board-review-badge-nav.mjs -- the
// regions are the shipped code itself, so the tests can't silently drift from
// what the librarian serves. Both functions are pure (no DOM/fetch), so the
// only injectable dependency is `esc`.

import { boardScript } from "./board-source.mjs";
import { fileURLToPath } from "node:url";
import { dirname, join } from "node:path";

const repoRoot = join(dirname(fileURLToPath(import.meta.url)), "..");
export const boardPath = join(repoRoot, "librarian", "assets", "board");

const html = boardScript();

/** Slices one marker-delimited region out of the real board script. */
function sliceRegion([BEGIN, END]) {
  const from = html.indexOf(BEGIN);
  const to = html.indexOf(END);
  if (from === -1 || to === -1 || to < from) {
    throw new Error(
      `board-exhausted-badges: could not find the ${BEGIN} / ${END} markers in ${boardPath}. ` +
        "If this logic moved, move the markers with it -- these tests are its only coverage.",
    );
  }
  return html.slice(from + BEGIN.length, to);
}

const REGIONS = {
  autoFix: ["// RALPHUS-AUTO-FIX-EXHAUSTED-BADGE:BEGIN", "// RALPHUS-AUTO-FIX-EXHAUSTED-BADGE:END"],
  rebase: ["// RALPHUS-REBASE-EXHAUSTED-NOTICE:BEGIN", "// RALPHUS-REBASE-EXHAUSTED-NOTICE:END"],
};

/** The raw board script (all chunks concatenated), for wiring assertions. */
export const boardSource = html;

/**
 * Builds the two sandboxed exhaustion indicators with an injectable `esc`
 * stub (defaults to the identity-ish passthrough used elsewhere in this test
 * suite).
 */
export function makeExhaustedBadges({ esc = (s) => String(s ?? "") } = {}) {
  // eslint-disable-next-line no-new-func -- evaluating the real shipped source is the point; see the header.
  const factory = new Function(
    "esc",
    `${sliceRegion(REGIONS.autoFix)}\n${sliceRegion(REGIONS.rebase)}\nreturn { autoFixExhaustedBadge, rebaseExhaustedNotice };`,
  );
  return factory(esc);
}
