// Loads the board's freshness-indicator logic (the "#updated" stamp and its
// "daemon unreachable" counterpart) out of the board chunk files
// (librarian/assets/board/) so it can be exercised under `node --test` with
// no browser and no build step.
//
// Same slice-the-real-source approach as ./board-review-badge-nav.mjs — the
// regions are the shipped code itself, so the tests can't silently drift from
// what the librarian serves. `markUpdated`/`markUnreachable` (20-util.js) are
// pulled in by their real source rather than hand-reimplemented, so a change
// to either can't drift unnoticed from what these tests exercise -- still
// true for `pollTasksTab`; `updateCounter` inlines their effect directly
// instead of calling them (RAL-390, see ./board-tasks-poll.mjs), so it also
// needs the shared `RALPHUS-TASKS-POLL-SEQ` region (`fetchTasksShared`) that
// region's sandbox pulls in alongside it.

import { boardScript } from "./board-source.mjs";

const html = boardScript();

/** Slices one marker-delimited region out of the real board script. */
function sliceRegion([BEGIN, END]) {
  const from = html.indexOf(BEGIN);
  const to = html.indexOf(END);
  if (from === -1 || to === -1 || to < from) {
    throw new Error(
      `board-freshness: could not find the ${BEGIN} / ${END} markers in the board chunks. ` +
        "If this logic moved, move the markers with it -- these tests are its only coverage.",
    );
  }
  return html.slice(from + BEGIN.length, to);
}

/** Pulls one `const NAME = ...;` declaration's real source out of the board script (first `};` close), so it can't drift from a hand-copied stand-in. */
function constSourceOf(name) {
  const m = html.match(new RegExp(`const ${name} = [\\s\\S]*?\\};`));
  if (!m) throw new Error(`board-freshness: could not find the ${name} definition in the board source.`);
  return m[0];
}
const FRESHNESS_HELPERS_SRC = `${constSourceOf("markUpdated")}\n${constSourceOf("markUnreachable")}`;

const REGIONS = {
  tasksSeqDecl: ["// RALPHUS-TASKS-POLL-SEQ:BEGIN", "// RALPHUS-TASKS-POLL-SEQ:END"],
  updateCounter: ["// RALPHUS-UPDATE-COUNTER:BEGIN", "// RALPHUS-UPDATE-COUNTER:END"],
  pollTasksTab: ["// RALPHUS-POLL-TASKS-TAB:BEGIN", "// RALPHUS-POLL-TASKS-TAB:END"],
};

/** The raw board script (all chunks concatenated), for source-text assertions (e.g. showTab). */
export const boardSource = html;

/** A `byId`-style element store: each id lazily gets a `{className, textContent}` record the sandboxed code can read/write, and the test can inspect afterward. */
function makeElements() {
  const els = {};
  const byId = (id) => (els[id] ||= { className: "", textContent: "" });
  return { els, byId };
}

/**
 * Builds the sandboxed `updateCounter` with a controllable fetch and the
 * board's real `markUpdated`/`markUnreachable` wired to an injectable `byId`.
 */
export function makeUpdateCounter() {
  const calls = { fetches: [] };
  /** @type {{url: string, resolve: (r: {json: () => Promise<any>}) => void, reject: (e: unknown) => void}[]} */
  const pendingFetches = [];
  const fetchImpl = (url) => {
    calls.fetches.push(url);
    return new Promise((resolve, reject) => { pendingFetches.push({ url, resolve, reject }); });
  };
  const { els, byId } = makeElements();
  const windowStub = {};
  const deps = {
    byId,
    window: windowStub,
    formatConcurrencyStatus: (running, maxConcurrent) => `Running ${running} / ${maxConcurrent === 0 ? "unlimited" : maxConcurrent}`,
  };
  // eslint-disable-next-line no-new-func -- evaluating the real shipped source is the point; see the header.
  const factory = new Function(
    "deps",
    "fetchImpl",
    `const { byId, window, formatConcurrencyStatus } = deps;
     const fetch = fetchImpl;
     var squads = [];
     ${FRESHNESS_HELPERS_SRC}
     ${sliceRegion(REGIONS.tasksSeqDecl)}
     ${sliceRegion(REGIONS.updateCounter)}
     return {
       updateCounter,
       state: () => ({ squads, daemonStatus: window._daemonStatus }),
     };`,
  );
  const api = factory(deps, fetchImpl);
  return { ...api, calls, pendingFetches, els };
}

/**
 * Builds the sandboxed `pollTasksTab` with a controllable fetch and the
 * board's real `markUpdated` wired to an injectable `byId`.
 */
export function makePollTasksTab({ pendingHash = null } = {}) {
  const calls = { renderTasksTab: 0, ttScrollSelectionIntoView: 0, fetches: [] };
  /** @type {{url: string, resolve: (r: {ok: boolean, json: () => Promise<any>}) => void, reject: (e: unknown) => void}[]} */
  const pendingFetches = [];
  const fetchImpl = (url) => {
    calls.fetches.push(url);
    return new Promise((resolve, reject) => { pendingFetches.push({ url, resolve, reject }); });
  };
  const { els, byId } = makeElements();
  const deps = {
    byId,
    taskTabSelFromUri: () => null,
    renderTasksTab: () => { calls.renderTasksTab++; },
    ttScrollSelectionIntoView: () => { calls.ttScrollSelectionIntoView++; },
  };
  // eslint-disable-next-line no-new-func -- evaluating the real shipped source is the point; see the header.
  const factory = new Function(
    "deps",
    "fetchImpl",
    `const { byId, taskTabSelFromUri, renderTasksTab, ttScrollSelectionIntoView } = deps;
     const fetch = fetchImpl;
     var taskTabPrIndex = null, taskTabSel = null, pendingHash = ${JSON.stringify(pendingHash)};
     ${FRESHNESS_HELPERS_SRC}
     ${sliceRegion(REGIONS.pollTasksTab)}
     return {
       pollTasksTab,
       state: () => ({ taskTabPrIndex, taskTabSel, pendingHash }),
     };`,
  );
  const api = factory(deps, fetchImpl);
  return { ...api, calls, pendingFetches, els };
}

/** Resolves one queued fetch with a JSON payload (defaults to an ok GET response). */
export function resolveJson(entry, data, { ok = true } = {}) {
  entry.resolve({ ok, json: async () => data });
}

/** Rejects one queued fetch (network failure). */
export function rejectFetch(entry, error = new Error("network error")) {
  entry.reject(error);
}
