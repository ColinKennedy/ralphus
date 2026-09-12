// Loads the RAL-390 out-of-order poll guards out of the board chunk files
// (librarian/assets/board/) so they can be exercised under `node --test` with
// no browser and no build step.
//
// Same slice-the-real-source approach as ./board-review-badge-nav.mjs (which
// covers the RAL-382 `reviewPollSeq` guard this one mirrors) -- the regions
// are the shipped code itself, so the tests can't silently drift from what
// the librarian serves.

import { boardScript } from "./board-source.mjs";

const html = boardScript();

/** Slices one marker-delimited region out of the real board script. */
function sliceRegion([BEGIN, END]) {
  const from = html.indexOf(BEGIN);
  const to = html.indexOf(END);
  if (from === -1 || to === -1 || to < from) {
    throw new Error(
      `board-tasks-poll: could not find the ${BEGIN} / ${END} markers in the board chunks. ` +
        "If this logic moved, move the markers with it -- these tests are its only coverage.",
    );
  }
  return html.slice(from + BEGIN.length, to);
}

const REGIONS = {
  tasksSeqDecl: ["// RALPHUS-TASKS-POLL-SEQ:BEGIN", "// RALPHUS-TASKS-POLL-SEQ:END"],
  updateCounter: ["// RALPHUS-UPDATE-COUNTER:BEGIN", "// RALPHUS-UPDATE-COUNTER:END"],
  pollTasks: ["// RALPHUS-POLL-TASKS:BEGIN", "// RALPHUS-POLL-TASKS:END"],
  whoamiSeqDecl: ["// RALPHUS-WHOAMI-POLL-SEQ:BEGIN", "// RALPHUS-WHOAMI-POLL-SEQ:END"],
  whoamiPoll: ["// RALPHUS-WHOAMI-POLL:BEGIN", "// RALPHUS-WHOAMI-POLL:END"],
};

/** A `byId`-style element store: each id lazily gets a `{className, textContent}` record the sandboxed code can read/write, and the test can inspect afterward. */
function makeElements() {
  const els = {};
  const byId = (id) => (els[id] ||= { className: "", textContent: "" });
  return { els, byId };
}

/**
 * Builds the sandboxed `updateCounter`/`pollTasks` pair (RAL-390) sharing one
 * `tasksPollSeq` ticket, with a controllable fetch: every fetch() queues a
 * `{ url, resolve, reject }` entry the test resolves manually, which is what
 * makes out-of-order poll completion deterministic.
 */
export function makeTasksPoll({ pendingHash = null, selectedSquadId = "s-existing", userIsSelecting = () => false, editing = false } = {}) {
  const calls = { renderAll: 0, renderSquads: 0, fetches: [] };
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
    userIsSelecting,
    renderAll: () => { calls.renderAll++; },
    renderSquads: () => { calls.renderSquads++; },
  };
  // eslint-disable-next-line no-new-func -- evaluating the real shipped source is the point; see the header.
  const factory = new Function(
    "deps",
    "fetchImpl",
    `const { byId, window, formatConcurrencyStatus, userIsSelecting, renderAll, renderSquads } = deps;
     const fetch = fetchImpl;
     var squads = [], pendingHash = ${JSON.stringify(pendingHash)}, selectedSquadId = ${JSON.stringify(selectedSquadId)}, editing = ${JSON.stringify(editing)};
     ${sliceRegion(REGIONS.tasksSeqDecl)}
     ${sliceRegion(REGIONS.updateCounter)}
     ${sliceRegion(REGIONS.pollTasks)}
     return {
       updateCounter,
       pollTasks,
       invalidateTasksFetch,
       state: () => ({ seq: tasksPollSeq, squads, daemonStatus: window._daemonStatus }),
     };`,
  );
  const api = factory(deps, fetchImpl);
  return { ...api, calls, pendingFetches, els };
}

/**
 * Builds the sandboxed `pollWhoAmI` (RAL-390) with a controllable fetch and
 * a state snapshot accessor, mirroring `makeTasksPoll` above.
 */
export function makeWhoAmIPoll({ tab = "machines" } = {}) {
  const calls = { applyAdminTabVisibility: 0, showTab: [], fetches: [] };
  /** @type {{url: string, resolve: (r: {ok: boolean, json: () => Promise<any>}) => void, reject: (e: unknown) => void}[]} */
  const pendingFetches = [];
  const fetchImpl = (url) => {
    calls.fetches.push(url);
    return new Promise((resolve, reject) => { pendingFetches.push({ url, resolve, reject }); });
  };
  const deps = {
    ADMIN_ONLY_TABS: ["machines", "triage", "projects", "users", "secrets"],
    applyAdminTabVisibility: () => { calls.applyAdminTabVisibility++; },
    showTab: (...a) => { calls.showTab.push(a); },
  };
  // eslint-disable-next-line no-new-func -- evaluating the real shipped source is the point; see the header.
  const factory = new Function(
    "deps",
    "fetchImpl",
    `const { ADMIN_ONLY_TABS, applyAdminTabVisibility, showTab } = deps;
     const fetch = fetchImpl;
     var currentUserName = null, currentUserIsAdmin = false, whoAmIResolved = false, tab = ${JSON.stringify(tab)};
     ${sliceRegion(REGIONS.whoamiSeqDecl)}
     ${sliceRegion(REGIONS.whoamiPoll)}
     return {
       pollWhoAmI,
       state: () => ({ seq: whoAmIPollSeq, currentUserName, currentUserIsAdmin, whoAmIResolved }),
     };`,
  );
  const api = factory(deps, fetchImpl);
  return { ...api, calls, pendingFetches };
}

/** Resolves one queued fetch with a JSON payload (defaults to an ok GET response, as both `/api/tasks` and `/api/whoami` return on success). */
export function resolveJson(entry, data, { ok = true } = {}) {
  entry.resolve({ ok, json: async () => data });
}
