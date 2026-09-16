// Loads the RAL-421 preview/confirm threshold-editor logic out of the board
// chunk files (librarian/assets/board/) so it can be exercised under
// `node --test` with no browser and no build step.
//
// Same slice-the-real-source approach as ./board-tasks-poll.mjs -- the
// regions are the shipped code itself, so these tests can't silently drift
// from what the librarian serves. Two regions: the pure preview helpers
// (key + summary + confirm-line markup) and the Triage-tab's
// preview/confirm/cancel handlers (the second editor entry point, the
// Projects-tab popup, uses the same helpers and the same request shapes).

import { boardScript } from "./board-source.mjs";

const html = boardScript();

/** Slices one marker-delimited region out of the real board script. */
function sliceRegion([BEGIN, END]) {
  const from = html.indexOf(BEGIN);
  const to = html.indexOf(END);
  if (from === -1 || to === -1 || to < from) {
    throw new Error(
      `board-triage-confirm: could not find the ${BEGIN} / ${END} markers in the board chunks. ` +
        "If this logic moved, move the markers with it -- these tests are its only coverage.",
    );
  }
  return html.slice(from + BEGIN.length, to);
}

const REGIONS = {
  preview: ["// RALPHUS-TRIAGE-PREVIEW:BEGIN", "// RALPHUS-TRIAGE-PREVIEW:END"],
  tabHandlers: ["// RALPHUS-TRIAGE-TAB-HANDLERS:BEGIN", "// RALPHUS-TRIAGE-TAB-HANDLERS:END"],
};

export const BEGIN_MARKERS = Object.fromEntries(
  Object.entries(REGIONS).map(([name, [BEGIN]]) => [name, BEGIN]),
);

/** A realistic draining preview the daemon would return for 7 pooled cells at a threshold of 3. */
export const DRAINING_PREVIEW = {
  project: "proj",
  triage_type: "bug",
  proposed_threshold: 3,
  clearing: false,
  pooled: 7,
  full_batches: 2,
  cells_drained: 6,
  cells_left: 1,
};

/** The default pool row `requestDrainTriagePool` reads (`proj`/`bug`, 7 pooled, threshold 3). */
export const DRAINABLE_POOL = { project: "proj", triage_type: "bug", count: 7, threshold: 3 };

/**
 * Builds the sandboxed preview/confirm pair: `previewPoolThreshold`,
 * `confirmPoolThreshold`, `cancelPoolThresholdPreview`, the RAL-449 manual
 * drain trio (`requestDrainTriagePool`, `confirmDrainTriagePool`,
 * `cancelDrainTriagePool`), plus the pure helpers (`triagePreviewKey`,
 * `triageThresholdPreviewEffect`, `triageThresholdConfirmLine`,
 * `triageDrainConfirmLine`). Every fetch is stubbed to resolve immediately;
 * the first call records the request so the test can assert URL, method,
 * and body, then returns `previewJson` for the `/preview` path,
 * `drainJson` for the `/drain` path, and `confirmJson` for the threshold
 * confirm path.
 * @param {{previewJson?: object, confirmJson?: object, drainJson?: object, pools?: object[]}} [opts]
 */
export function makeTriageConfirm({
  previewJson = DRAINING_PREVIEW,
  confirmJson = { ok: true, reviews_created: 2, cells_drained: 6, cells_left: 1 },
  drainJson = { guardian_id: "guardian-1" },
  pools = [DRAINABLE_POOL],
} = {}) {
  const calls = {
    fetches: [],
    renderTriage: 0,
    pollTriage: 0,
    responseError: 0,
  };
  const esc = (s) => String(s ?? "").replace(/[&<>"']/g, (c) => ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;" })[c]);
  const fetchImpl = (url, init) => {
    calls.fetches.push({ url, init });
    const body = url.includes("/drain") ? drainJson : (url.includes("/preview") ? previewJson : confirmJson);
    return Promise.resolve({ ok: true, json: async () => body });
  };
  const responseError = async (_r, message) => { calls.responseError++; return message; };
  const deps = {
    esc,
    fetch: fetchImpl,
    renderTriage: () => { calls.renderTriage++; },
    pollTriage: async () => { calls.pollTriage++; },
    responseError,
    pools,
  };
  // eslint-disable-next-line no-new-func -- evaluating the real shipped source is the point; see the header.
  const factory = new Function(
    "deps",
    `const { esc, fetch, renderTriage, pollTriage, responseError } = deps;
     var triageError = "";
     var triageThresholdPreviews = {};
     var triagePools = deps.pools;
     var triageDrainConfirms = {};
     ${sliceRegion(REGIONS.preview)}
     ${sliceRegion(REGIONS.tabHandlers)}
     return {
       previewPoolThreshold,
       confirmPoolThreshold,
       cancelPoolThresholdPreview,
       requestDrainTriagePool,
       confirmDrainTriagePool,
       cancelDrainTriagePool,
       triagePreviewKey,
       triageThresholdPreviewEffect,
       triageThresholdConfirmLine,
       triageDrainConfirmLine,
       triageError: () => triageError,
       previews: () => triageThresholdPreviews,
       drainConfirms: () => triageDrainConfirms,
     };`,
  );
  const api = factory(deps);
  api.calls = calls;
  return api;
}

/** A fake event whose target row carries one threshold input. */
export function makeRowEvent(rawValue) {
  const input = { value: rawValue };
  const row = { querySelector: (sel) => (sel === ".pool-threshold-input" ? input : null) };
  const target = { closest: (sel) => (sel === "tr" ? row : null) };
  return { target, input };
}