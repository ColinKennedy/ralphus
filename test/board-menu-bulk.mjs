// Loads the RAL-508 bulk context-menu helpers out of the board chunk files
// (librarian/assets/board/) so they can be exercised under `node --test`
// with no browser and no build step.
//
// Same slice-the-real-source approach as ./board-tt-multiselect.mjs -- the
// regions are the shipped code itself, so these tests can't silently drift
// from what the librarian serves. Regions: the shared bulk primitives
// (menuActionTargets / bulkEligibleSplit / bulkActEach / reportBulkOutcome /
// bulkNameList, in 20-util.js), the squad bulk-activate / bulk-retry /
// bulk-restart-confirm / bulk-cancel-preview / bulk-cancel-confirm /
// bulk-delete / hide-menu-routing actions (25-chrome.js, 40-dialogs.js), the
// review bulk-cancel / bulk-reopen / bulk-delete / hide / context-menu
// actions (65-reviews.js), and the Set Status bulk route (05-engines.js's
// menu-item handler) plus bulkSetStatus / doPickStatus (40-dialogs.js).

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
      `board-menu-bulk: could not find the ${BEGIN} / ${END} markers in ${boardPath}. ` +
        "If this logic moved, move the markers with it -- these tests are its only coverage.",
    );
  }
  return html.slice(from + BEGIN.length, to);
}

const REGIONS = {
  helpers: ["// RALPHUS-MENU-BULK:BEGIN", "// RALPHUS-MENU-BULK:END"],
  squadActivate: ["// RALPHUS-SQUAD-BULK-ACTIVATE:BEGIN", "// RALPHUS-SQUAD-BULK-ACTIVATE:END"],
  squadRetry: ["// RALPHUS-SQUAD-BULK-RETRY:BEGIN", "// RALPHUS-SQUAD-BULK-RETRY:END"],
  reviewCancel: ["// RALPHUS-REVIEW-BULK-CANCEL:BEGIN", "// RALPHUS-REVIEW-BULK-CANCEL:END"],
  reviewReopen: ["// RALPHUS-REVIEW-REOPEN:BEGIN", "// RALPHUS-REVIEW-REOPEN:END"],
  reviewDelete: ["// RALPHUS-REVIEW-BULK-DELETE:BEGIN", "// RALPHUS-REVIEW-BULK-DELETE:END"],
  reviewHide: ["// RALPHUS-REVIEW-HIDE:BEGIN", "// RALPHUS-REVIEW-HIDE:END"],
  squadMenu: ["// RALPHUS-SQUAD-MENU:BEGIN", "// RALPHUS-SQUAD-MENU:END"],
  reviewMenu: ["// RALPHUS-REVIEW-MENU:BEGIN", "// RALPHUS-REVIEW-MENU:END"],
  bulkDelete: ["// RALPHUS-BULK-DELETE:BEGIN", "// RALPHUS-BULK-DELETE:END"],
  squadHideRoute: ["// RALPHUS-SQUAD-HIDE-ROUTE:BEGIN", "// RALPHUS-SQUAD-HIDE-ROUTE:END"],
  bulkCancelPreview: ["// RALPHUS-BULK-CANCEL-PREVIEW:BEGIN", "// RALPHUS-BULK-CANCEL-PREVIEW:END"],
  bulkCancelConfirm: ["// RALPHUS-BULK-CANCEL-CONFIRM:BEGIN", "// RALPHUS-BULK-CANCEL-CONFIRM:END"],
  bulkRestartConfirm: ["// RALPHUS-BULK-RESTART-CONFIRM:BEGIN", "// RALPHUS-BULK-RESTART-CONFIRM:END"],
  setStatusPick: ["// RALPHUS-SET-STATUS-PICK:BEGIN", "// RALPHUS-SET-STATUS-PICK:END"],
  bulkSetStatus: ["// RALPHUS-BULK-SET-STATUS:BEGIN", "// RALPHUS-BULK-SET-STATUS:END"],
  setStatusRoute: ["// RALPHUS-SET-STATUS-ROUTE:BEGIN", "// RALPHUS-SET-STATUS-ROUTE:END"],
  squadRetryRoute: ["// RALPHUS-SQUAD-RETRY-ROUTE:BEGIN", "// RALPHUS-SQUAD-RETRY-ROUTE:END"],
  squadRestartRoute: ["// RALPHUS-SQUAD-RESTART-ROUTE:BEGIN", "// RALPHUS-SQUAD-RESTART-ROUTE:END"],
  squadCancelRoute: ["// RALPHUS-SQUAD-CANCEL-ROUTE:BEGIN", "// RALPHUS-SQUAD-CANCEL-ROUTE:END"],
  squadDeleteRoute: ["// RALPHUS-SQUAD-DELETE-ROUTE:BEGIN", "// RALPHUS-SQUAD-DELETE-ROUTE:END"],
};

/** A stub Response-shaped object `bulkActEach`/`responseError` can consume. */
const response = (ok, message = "boom") => ({ ok, json: async () => ({ error: { message } }) });

/** A minimal `esc` matching 20-util.js's, for harnesses that render HTML. */
const esc = (s) => String(s).replace(/&/g, "&amp;").replace(/</g, "&lt;").replace(/>/g, "&gt;").replace(/"/g, "&quot;");

/** A minimal fake document/window/event trio for the menu-rendering harnesses. */
function makeMenuDom() {
  const appended = [];
  const menuEl = { style: {} };
  const document = {
    createElement: () => menuEl,
    body: { appendChild: (el) => appended.push(el) },
  };
  const window = { innerWidth: 2000, innerHeight: 1200 };
  const event = { preventDefault() {}, stopPropagation() {}, clientX: 5, clientY: 5 };
  return { document, window, event, menuEl, appended };
}

/**
 * Evaluates the shared bulk primitives (the RALPHUS-MENU-BULK region) with
 * injectable `responseError`/`notify`/`tick` stubs and returns them plus a
 * `calls` recorder.
 */
export function makeBulkHelpers({ responseErrorMessage = "srv error" } = {}) {
  const calls = { notifications: [], ticks: 0 };
  const deps = {
    responseError: async (_resp, fallback) => (responseErrorMessage === null ? fallback : responseErrorMessage),
    notify: (kind, msg) => { calls.notifications.push({ kind, msg }); },
    tick: () => { calls.ticks++; },
  };
  // eslint-disable-next-line no-new-func -- evaluating the real shipped source is the point; see the header.
  const factory = new Function(
    "deps",
    `const { responseError, notify, tick } = deps;
     ${sliceRegion(REGIONS.helpers)}
     return { menuActionTargets, bulkEligibleSplit, bulkActEach, reportBulkOutcome, bulkNameList };`,
  );
  const api = factory(deps);
  return { ...api, calls };
}

const ACTION_EXPORTS = {
  squadActivate: "bulkActivateSquads",
  squadRetry: "bulkRetrySquads",
  reviewCancel: "bulkCancelReviews",
  reviewReopen: "bulkReopenReviews",
  reviewDelete: "bulkDeleteReviews",
  reviewHide: "setReviewHiddenFromMenu, bulkHideReviews, bulkUnhideReviews",
  reviewUnhide: "bulkUnhideReviews",
  squadDelete: "bulkDelete",
  squadHideRoute: "setSquadHiddenFromMenu",
  bulkCancelPreview: "showBulkCancelPreview",
  bulkCancelConfirm: "confirmBulkCancelSquads",
  bulkRestartConfirm: "confirmBulkRestart",
  setStatusPick: "doPickStatus",
  bulkSetStatus: "bulkSetStatus",
};

const ACTION_REGIONS = {
  squadActivate: [REGIONS.helpers, REGIONS.squadActivate],
  squadRetry: [REGIONS.helpers, REGIONS.squadRetry],
  reviewCancel: [REGIONS.helpers, REGIONS.reviewCancel],
  reviewReopen: [REGIONS.helpers, REGIONS.reviewReopen],
  reviewDelete: [REGIONS.helpers, REGIONS.reviewDelete],
  reviewHide: [REGIONS.helpers, REGIONS.reviewHide],
  reviewUnhide: [REGIONS.helpers, REGIONS.reviewHide],
  squadDelete: [REGIONS.helpers, REGIONS.bulkDelete],
  squadHideRoute: [REGIONS.helpers, REGIONS.squadHideRoute],
  bulkCancelPreview: [REGIONS.helpers, REGIONS.bulkCancelPreview],
  bulkCancelConfirm: [REGIONS.helpers, REGIONS.bulkCancelConfirm],
  bulkRestartConfirm: [REGIONS.helpers, REGIONS.bulkRestartConfirm],
  setStatusPick: [REGIONS.helpers, REGIONS.setStatusPick],
  bulkSetStatus: [REGIONS.helpers, REGIONS.bulkSetStatus],
};

/**
 * Builds a sandbox holding the real bulk primitives plus one of the bulk
 * action functions, with every outside dependency stubbed and recorded in
 * `calls`.
 * @param {{action: keyof typeof ACTION_EXPORTS, squads?: object, guardians?: object, postOk?: boolean, failIds?: string[], confirmResult?: boolean, promptResult?: string|null, cancellable?: string[], multiSel?: Set<string>, guardianMultiSel?: Set<string>, hiddenGuardianIds?: Set<string>, cancelPreviews?: object, note?: string|null, applyAllToAll?: boolean}} opts
 */
export function makeBulkAction({
  action, squads = {}, guardians = {}, postOk = true, failIds = [], confirmResult = true, promptResult = "retry",
  cancellable = ["collecting", "merging", "merge_failed", "merge_stopped", "in_review", "merged"],
  multiSel = new Set(), guardianMultiSel = new Set(), hiddenGuardianIds = new Set(),
  cancelPreviews = {}, note = null, applyAllToAll = false, statusItems = [],
} = {}) {
  const calls = { notifications: [], ticks: 0, posts: [], postBodies: [], dels: [], fetches: [], confirmTexts: [], promptTexts: [], closeModals: 0, renderReviews: 0, bulkCalls: [], singleHides: [], modalHtml: undefined, pickerItems: undefined, statusItems: undefined };
  const post = (path, body) => {
    calls.posts.push(path);
    calls.postBodies.push(body);
    const segs = path.split("/").filter(Boolean);
    const id = segs[1] === "hidden" ? segs.at(-1) : path.endsWith("/cancel/preview") ? segs.at(-3) : segs.at(-2);
    if (action === "bulkCancelPreview" && path.endsWith("/cancel/preview")) {
      return Promise.resolve({ ok: postOk && !failIds.includes(id), json: async () => ({ squads: cancelPreviews[id] || [] }) });
    }
    return Promise.resolve(response(postOk && !failIds.includes(id), "daemon said no"));
  };
  const del = (path) => {
    calls.dels.push(path);
    const id = path.split("/").filter(Boolean).at(-1);
    return Promise.resolve(response(postOk && !failIds.includes(id), "daemon said no"));
  };
  const fetchImpl = (url) => {
    calls.fetches.push(url);
    const id = url.split("/").filter(Boolean).at(-2);
    return Promise.resolve(response(!failIds.includes(id), "daemon said no"));
  };
  const deps = {
    responseError: async (_resp, fallback) => "daemon said no",
    notify: (kind, msg) => { calls.notifications.push({ kind, msg }); },
    tick: () => { calls.ticks++; },
    post,
    del,
    fetch: fetchImpl,
    confirm: (text) => { calls.confirmTexts.push(text); return confirmResult; },
    prompt: (text) => { calls.promptTexts.push(text); return promptResult; },
    findSquad: (id) => squads[id],
    squadLabelOf: (id) => squads[id]?.label || id,
    SQUAD_TERMINAL_STATES: ["done", "failed", "cancelled"],
    guardians: Object.values(guardians),
    reviewLabelOf: (id) => guardians[id]?.name || id,
    G_CANCELLABLE: cancellable,
    esc,
    closeSquadMenu: () => {},
    multiSel,
    guardianMultiSel,
    hiddenGuardianIds,
    selectedSquadId: "sq-selected",
    selectedGuardian: "g-selected",
    closeModal: () => { calls.closeModals++; },
    renderReviews: () => { calls.renderReviews++; },
    bulkHideSquads: () => { calls.bulkCalls.push("bulkHideSquads"); },
    bulkUnhideSquads: () => { calls.bulkCalls.push("bulkUnhideSquads"); },
    setSquadHidden: (id, hide) => { calls.singleHides.push([id, hide]); },
    setReviewHidden: (id, hide) => { calls.singleHides.push([id, hide]); },
    document: {
      getElementById: (name) => {
        if (name === "restart-note-input") return note === null ? null : { value: note };
        if (name === "restart-note-apply-all") return note === null ? null : { checked: applyAllToAll };
        return null;
      },
    },
    byId: () => { const el = {}; Object.defineProperty(el, "innerHTML", { set(v) { calls.modalHtml = v; }, get() { return ""; } }); return el; },
    statusItems,
    openStatusPicker: (_e, items) => { calls.pickerItems = items; },
    closeStatusPicker: () => {},
    IRREVERSIBLE_STATES: new Set(["done", "failed", "cancelled"]),
    showCancelPreview: async () => {},
    reportGraphActionOutcome: () => {},
    tab: "tasks",
    pollQueue: () => {},
    event: { stopPropagation() {} },
  };
  const regionSlices = ACTION_REGIONS[action].map(sliceRegion);
  const exported = ACTION_EXPORTS[action];
  // eslint-disable-next-line no-new-func -- evaluating the real shipped source is the point; see the header.
  const factory = new Function(
    "deps",
    `const { responseError, notify, tick, post, del, fetch, confirm, prompt, findSquad, squadLabelOf, guardians, reviewLabelOf, G_CANCELLABLE, esc, closeSquadMenu, closeModal, renderReviews, bulkHideSquads, bulkUnhideSquads, setSquadHidden, setReviewHidden, document, byId, openStatusPicker, closeStatusPicker, showCancelPreview, reportGraphActionOutcome, pollQueue, event } = deps;
     var SQUAD_TERMINAL_STATES = deps.SQUAD_TERMINAL_STATES;
     var IRREVERSIBLE_STATES = deps.IRREVERSIBLE_STATES;
     var tab = deps.tab;
     var _statusPickerItems = deps.statusItems;
     var multiSel = deps.multiSel;
     var guardianMultiSel = deps.guardianMultiSel;
     var hiddenGuardianIds = deps.hiddenGuardianIds;
     var selectedSquadId = deps.selectedSquadId;
     var selectedGuardian = deps.selectedGuardian;
     ${regionSlices.join("\n")}
     return { menuActionTargets, bulkEligibleSplit, bulkActEach, reportBulkOutcome, bulkNameList, ${exported},
       state: () => ({ selectedSquadId, selectedGuardian, multiSelSize: multiSel.size, guardianMultiSelSize: guardianMultiSel.size, hidden: [...hiddenGuardianIds] }) };`,
  );
  const api = factory(deps);
  return { ...api, calls };
}

/**
 * Renders the real squad context menu (openSquadMenu) for one clicked squad
 * under a given multi-selection, returning the menu element's HTML.
 * @param {{squads?: object, selection?: string[], clickedId: string, watching?: string[], hidden?: string[]}} opts
 */
export function makeSquadMenu({ squads = {}, selection = [], clickedId, watching = [], hidden = [] } = {}) {
  const { document, window, event, menuEl } = makeMenuDom();
  const deps = {
    esc, findSquad: (id) => squads[id], multiSel: new Set(selection),
    isWatching: (uri) => watching.includes(uri), hiddenSquadIds: new Set(hidden),
    closeSquadMenu: () => {}, document, window, event, clickedId,
  };
  // eslint-disable-next-line no-new-func -- evaluating the real shipped source is the point; see the header.
  const factory = new Function(
    "deps",
    `const { esc, findSquad, multiSel, isWatching, hiddenSquadIds, closeSquadMenu, document, window, event, clickedId, menuEl } = deps;
     var SQUAD_TERMINAL_STATES = deps.SQUAD_TERMINAL_STATES;
     ${sliceRegion(REGIONS.helpers)}
     ${sliceRegion(REGIONS.squadMenu)}
     openSquadMenu(event, clickedId);
     return menuEl;`,
  );
  return factory({ ...deps, SQUAD_TERMINAL_STATES: ["done", "failed", "cancelled"], menuEl });
}

/**
 * Renders the real review context menu (openReviewMenu) for one clicked
 * review under a given multi-selection, returning the menu element.
 * @param {{guardians?: object, selection?: string[], clickedId: string, watching?: string[], hidden?: string[], cancellable?: string[]}} opts
 */
export function makeReviewMenu({ guardians = {}, selection = [], clickedId, watching = [], hidden = [], cancellable = ["collecting", "merging", "merge_failed", "merge_stopped", "in_review", "merged"] } = {}) {
  const { document, window, event, menuEl } = makeMenuDom();
  const deps = {
    esc, guardians: Object.values(guardians), guardianMultiSel: new Set(selection),
    G_CANCELLABLE: cancellable, isWatching: (uri) => watching.includes(uri), hiddenGuardianIds: new Set(hidden),
    closeSquadMenu: () => {}, document, window, event, clickedId,
  };
  // eslint-disable-next-line no-new-func -- evaluating the real shipped source is the point; see the header.
  const factory = new Function(
    "deps",
    `const { esc, guardians, guardianMultiSel, G_CANCELLABLE, isWatching, hiddenGuardianIds, closeSquadMenu, document, window, event, clickedId, menuEl } = deps;
     ${sliceRegion(REGIONS.helpers)}
     ${sliceRegion(REGIONS.reviewMenu)}
     openReviewMenu(event, clickedId);
     return menuEl;`,
  );
  return factory({ ...deps, menuEl });
}

/**
 * Fires the real menu-action routing function for one clicked squad under a
 * given multi-selection, recording whether it took the bulk path (with which
 * ids) or the single-item path. Covers the RAL-508 routing in retrySquad /
 * restartSquad / cancelSquad (25-chrome.js), deleteSquad (40-dialogs.js), and
 * 05-engines.js's openStatusPickerForSquadMenuItem handler.
 * @param {{region: keyof typeof ROUTE_HANDLERS, squads?: object, selection?: string[], clickedId: string, confirmResult?: boolean}} opts
 */
const ROUTE_HANDLERS = {
  setStatusRoute: "openStatusPickerForSquadMenuItem",
  squadRetryRoute: "retrySquad",
  squadRestartRoute: "restartSquad",
  squadCancelRoute: "cancelSquad",
  squadDeleteRoute: "deleteSquad",
};
export function makeSquadMenuRoute({ region, squads = {}, selection = [], clickedId, confirmResult = false } = {}) {
  const calls = { bulk: [], single: [], confirms: [], prompts: [], posts: [], dels: [] };
  const event = { stopPropagation() {} };
  const deps = {
    closeSquadMenu: () => {},
    event,
    clickedId,
    multiSel: new Set(selection),
    selectedSquadId: null,
    findSquad: (id) => squads[id],
    confirm: (text) => { calls.confirms.push(text); return confirmResult; },
    prompt: (text) => { calls.prompts.push(text); return "retry"; },
    post: async (path) => { calls.posts.push(path); return response(true); },
    del: async (path) => { calls.dels.push(path); return response(true); },
    tick: () => {},
    bulkRetrySquads: async (ids) => { calls.bulk.push(["bulkRetrySquads", ids]); },
    bulkRestartSquads: async (ids) => { calls.bulk.push(["bulkRestartSquads", ids]); },
    showBulkCancelPreview: async (ids) => { calls.bulk.push(["showBulkCancelPreview", ids]); },
    bulkDelete: async () => { calls.bulk.push(["bulkDelete"]); },
    bulkSetStatus: async () => { calls.bulk.push(["bulkSetStatus"]); },
    openStatusPickerForSquad: (_e, id) => { calls.single.push(["openStatusPickerForSquad", id]); },
    showRestartPreview: async (_msg, url) => { calls.single.push(["showRestartPreview", url]); },
    showCancelPreview: async (_msg, url) => { calls.single.push(["showCancelPreview", url]); },
  };
  // eslint-disable-next-line no-new-func -- evaluating the real shipped source is the point; see the header.
  const callSnippet = region === "setStatusRoute"
    ? // 05-engines.js's handler is an object-literal entry: wrap and dispatch through it.
      `var HANDLERS = { ${sliceRegion(REGIONS.setStatusRoute)} menuEl: null };
     await HANDLERS.openStatusPickerForSquadMenuItem(event, { squadId: deps.clickedId });`
    : // the squad-menu action functions are plain (async) declarations: evaluate then call.
      `${sliceRegion(REGIONS[region])}
     await ${ROUTE_HANDLERS[region]}(deps.clickedId);`;
  const factory = new Function(
    "deps",
    `const { closeSquadMenu, event, confirm, prompt, post, del, tick, findSquad, bulkRetrySquads, bulkRestartSquads, showBulkCancelPreview, bulkDelete, bulkSetStatus, openStatusPickerForSquad, showRestartPreview, showCancelPreview } = deps;
     ${sliceRegion(REGIONS.helpers)}
     var multiSel = deps.multiSel;
     var selectedSquadId = deps.selectedSquadId;
     return (async () => {
       ${callSnippet}
       return deps.calls;
     })();`,
  );
  deps.calls = calls;
  return factory(deps);
}
/**
 * Fires the real 05-engines.js click handler for the squad menu's Set Status
 * item under a given multi-selection, recording whether it opened the bulk
 * picker (one picker item per selected squad) or the single-squad picker.
 * @param {{squads?: object, selection?: string[], clickedId: string}} opts
 */
export function makeStatusRoute({ squads = {}, selection = [], clickedId } = {}) {
  const calls = { pickerItems: undefined, singlePickerId: undefined, bulkCalled: 0 };
  const event = { stopPropagation() {} };
  const deps = {
    squads, clickedId, event,
    closeSquadMenu: () => {},
    openStatusPickerForSquad: (_e, id) => { calls.singlePickerId = id; },
    bulkSetStatus: async (e) => {
      calls.bulkCalled++;
      // Mirror the real bulkSetStatus body so the recorded items show what
      // the picker would have been scoped to.
      const items = [...deps.multiSel].map((id) => ({ squadId: id, label: deps.findSquad(id)?.label || id }));
      if (items.length) calls.pickerItems = items;
      void e;
    },
  };
  deps.findSquad = (id) => squads[id];
  deps.multiSel = new Set(selection);
  // eslint-disable-next-line no-new-func -- evaluating the real shipped source is the point; see the header.
  const factory = new Function(
    "deps",
    `const { closeSquadMenu, openStatusPickerForSquad, bulkSetStatus, event } = deps;
     ${sliceRegion(REGIONS.helpers)}
     var multiSel = deps.multiSel;
     var HANDLERS = { ${sliceRegion(REGIONS.setStatusRoute)} menuEl: null };
     HANDLERS.openStatusPickerForSquadMenuItem(event, { squadId: deps.clickedId });
     return deps.calls;`,
  );
  deps.calls = calls;
  return factory(deps);
}
