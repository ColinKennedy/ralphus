// Loads the RAL-382 review-badge navigation logic out of the board chunk files
// (librarian/assets/board/) so it can be exercised under `node --test` with no
// browser and no build step.
//
// Same slice-the-real-source approach as ./board-task-tab-logic.mjs and
// ./board-simple-tab.mjs — the regions are the shipped code itself, so the
// tests can't silently drift from what the librarian serves. The three regions
// are deliberately small and their DOM/fetch/global dependencies are provided
// as injectable stubs by the test (see the factories below).

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
      `board-review-badge-nav: could not find the ${BEGIN} / ${END} markers in ${boardPath}. ` +
        "If this logic moved, move the markers with it -- these tests are its only coverage.",
    );
  }
  return html.slice(from + BEGIN.length, to);
}

const REGIONS = {
  badges: ["// RALPHUS-TT-REVIEW-BADGES:BEGIN", "// RALPHUS-TT-REVIEW-BADGES:END"],
  poll: ["// RALPHUS-REVIEW-POLL:BEGIN", "// RALPHUS-REVIEW-POLL:END"],
  gotoReview: ["// RALPHUS-GOTO-REVIEW:BEGIN", "// RALPHUS-GOTO-REVIEW:END"],
};

/** The raw board script (all chunks concatenated), for wiring assertions. */
export const boardSource = html;

/** `ttReviewPrBadgesHtml` reads the module-level `G_COLORS` and `TT_PR_COLORS` maps
 * (defined in 65-reviews.js and 10-tab-registry.js respectively) out of shared script
 * scope — pull their real definitions out of the source so the sandboxed badge factory
 * has them too, instead of hand-maintaining copies that could drift. */
function constSourceOf(name) {
  const m = boardSource.match(new RegExp(`const ${name} = \\{[^}]*\\};`));
  if (!m) throw new Error(`board-review-badge-nav: could not find the ${name} definition in the board source.`);
  return m[0];
}
const BADGE_GLOBALS_SRC = `${constSourceOf("G_COLORS")}\n${constSourceOf("TT_PR_COLORS")}`;

/**
 * Builds the Tasks-tab Review/PR badge lane renderer with injectable `esc`/`cvar`
 * stubs. `ttOpenPr` needs `window` only if invoked — these tests never invoke it.
 */
export function makeBadgeRenderer({ esc = (s) => String(s ?? ""), cvar = (n) => n } = {}) {
  // eslint-disable-next-line no-new-func -- evaluating the real shipped source is the point; see the header.
  const factory = new Function("esc", "cvar", `${BADGE_GLOBALS_SRC}\n${sliceRegion(REGIONS.badges)}\nreturn { ttReviewPrBadgesHtml, ttOpenPr };`);
  return factory(esc, cvar);
}

/**
 * Builds the sandboxed `gotoReview` (RAL-382) with injectable collaborators and
 * a state snapshot accessor. `renderReviewDetail` here is only a call recorder —
 * the test asserts gotoReview triggers it synchronously; the placeholder choice
 * inside the real one is pinned separately by source assertions.
 */
export function makeGotoReview({ showTab = () => {}, findGuardian = () => false } = {}) {
  const calls = { showTab: [], renderReviews: 0, renderReviewDetail: 0 };
  // eslint-disable-next-line no-new-func -- evaluating the real shipped source is the point; see the header.
  const factory = new Function(
    "deps",
    `const { showTab, findGuardian, renderReviews, renderReviewDetail } = deps;
     var selectedGuardian = null, revealedGuardianId = null, reviewDetailLoading = null;
     ${sliceRegion(REGIONS.gotoReview)}
     return {
       gotoReview,
       state: () => ({ selectedGuardian, revealedGuardianId, reviewDetailLoading }),
     };`,
  );
  const api = factory({
    showTab: (...a) => { calls.showTab.push(...a); },
    findGuardian,
    renderReviews: () => { calls.renderReviews++; },
    renderReviewDetail: () => { calls.renderReviewDetail++; },
  });
  return { ...api, calls };
}

/**
 * Builds the sandboxed `pollReviews` (RAL-382) with injectable collaborators and
 * a controllable fetch: every fetch() queues a `{ url, resolve, reject }` entry
 * the test resolves manually, which is what makes out-of-order poll completion
 * deterministic.
 */
export function makePollReviews({ userIsSelecting = () => false, pendingHash = null, initialGuardians = [], slowRefresh = null } = {}) {
  const calls = { renderReviews: 0, renderReviewDetail: 0, fetches: [] };
  /** @type {{url: string, resolve: (r: {json: () => Promise<any>}) => void, reject: (e: unknown) => void}[]} */
  const pendingFetches = [];
  const fetchImpl = (url) => {
    calls.fetches.push(url);
    return new Promise((resolve, reject) => { pendingFetches.push({ url, resolve, reject }); });
  };
  const deps = {
    checkGuardianNotices: () => {},
    byId: () => ({ className: "" }),
    findGuardian: (id) => (callState().guardians || []).find((g) => g.id === id),
    visibleGuardians: () => [],
    syncHash: () => {},
    // `slowRefresh` lets a test park a poll inside its post-list Promise.all —
    // the await point the second (pre-render) sequence guard protects. Only the
    // *first* call is gated: it stands in for the older poll that gets parked,
    // while later calls (the newer poll that overtakes it) must complete freely
    // or the newer poll would deadlock waiting on the same unresolved promise.
    refreshExpandedBranchMessages: (() => {
      let calls = 0;
      return async () => { calls++; if (slowRefresh && calls === 1) await slowRefresh; };
    })(),
    pollBranchConflicts: async () => {},
    pollPullRequests: async () => {},
    pollPrErrors: async () => {},
    preserveUserState: (_el, fn) => fn(),
    renderReviews: () => { calls.renderReviews++; },
    renderReviewDetail: () => { calls.renderReviewDetail++; },
    userIsSelecting,
    document: { getElementById: () => ({}) },
  };
  // eslint-disable-next-line no-new-func -- evaluating the real shipped source is the point; see the header.
  const factory = new Function(
    "deps",
    "fetchImpl",
    `const { checkGuardianNotices, byId, findGuardian, visibleGuardians, syncHash, refreshExpandedBranchMessages,
             pollBranchConflicts, pollPullRequests, pollPrErrors, preserveUserState, renderReviews, renderReviewDetail,
             userIsSelecting, document } = deps;
     const fetch = fetchImpl;
     var guardians = ${JSON.stringify(initialGuardians)}, selectedGuardian = null, revealedGuardianId = null,
         pendingHash = ${JSON.stringify(pendingHash)}, reviewDetailLoading = null;
     ${sliceRegion(REGIONS.poll)}
     return {
       pollReviews,
       state: () => ({ seq: reviewPollSeq, guardians, selectedGuardian, revealedGuardianId, reviewDetailLoading, pendingHash }),
     };`,
  );
  /** Re-read via api.state() — kept in closure by the factory. */
  const callState = () => api.state();
  const api = factory(deps, fetchImpl);
  return { ...api, calls, pendingFetches };
}

/** Resolves one queued fetch with a JSON payload. */
export function resolveJson(entry, data) {
  entry.resolve({ ok: true, json: async () => data });
}
