// Loads the Tasks toolbar's and Reviews sidebar's PR-status dropdown wiring
// (RAL-474) straight out of librarian/assets/board/, fused with the real
// shared Status dropdown component (RAL-475, RALPHUS-STATUS-DROPDOWN region
// -- see ./board-status-dropdown.mjs's header) so open/select/dismiss here
// exercises the same renderStatusDropdown/statusDropdownToggleMenu/...  code
// paths production uses, not a mock of them. Same slice-the-real-source
// approach as ./board-project-filter-menu.mjs -- this logic touches
// `document` directly, which is why it needs its own DOM stubs.

import { boardScript } from "./board-source.mjs";

const STATUS_DROPDOWN_REGION = ["// RALPHUS-STATUS-DROPDOWN:BEGIN", "// RALPHUS-STATUS-DROPDOWN:END"];
const REGIONS = {
  tasks: ["// RALPHUS-TT-PR-STATUS-FILTER:BEGIN", "// RALPHUS-TT-PR-STATUS-FILTER:END"],
  reviews: ["// RALPHUS-REVIEW-PR-STATUS-FILTER:BEGIN", "// RALPHUS-REVIEW-PR-STATUS-FILTER:END"],
};

function sliceRegion([BEGIN, END]) {
  const html = boardScript();
  const from = html.indexOf(BEGIN);
  const to = html.indexOf(END);
  if (from === -1 || to === -1 || to < from) {
    throw new Error(
      `board-pr-status-filter: could not find the ${BEGIN} / ${END} markers in librarian/assets/board/. ` +
        "If this logic moved, move the markers with it -- these tests are its only coverage.",
    );
  }
  return html.slice(from + BEGIN.length, to);
}

/** Mirrors 10-tab-registry.js's TT_PR_CI_COLORS -- the three PR CI statuses' documented color roles (RAL-395). */
const TT_PR_CI_COLORS = { passing: "--done", failing: "--failed", pending: "--pending" };

/** A minimal fake element/document -- same shape as ./board-status-dropdown.mjs's, since the combined source touches the same document APIs. */
function makeFakeDocument() {
  const byIdMap = new Map();

  function makeElement() {
    const attrs = new Map();
    const el = {
      id: "",
      className: "",
      innerHTML: "",
      style: {},
      _focused: false,
      setAttribute(name, value) { attrs.set(name, String(value)); },
      getAttribute(name) { return attrs.has(name) ? attrs.get(name) : null; },
      focus() { el._focused = true; },
      addEventListener() {},
      querySelector() { return makeElement(); },
      getBoundingClientRect() { return { left: 0, right: 0, top: 0, bottom: 0 }; },
      remove() { if (el.id) byIdMap.delete(el.id); },
    };
    return el;
  }

  function selectorMatches(selector, el) {
    const classMatch = selector.match(/^\.([\w-]+)/);
    if (classMatch && el.className !== classMatch[1]) return false;
    const idPrefixMatch = selector.match(/\[id\^='([^']+)'\]/);
    if (idPrefixMatch && !el.id.startsWith(idPrefixMatch[1])) return false;
    return true;
  }

  const doc = {
    getElementById: (id) => byIdMap.get(id) || null,
    createElement: () => makeElement(),
    querySelectorAll: (selector) => [...byIdMap.values()].filter((el) => selectorMatches(selector, el)),
    body: { appendChild: (el) => { if (el.id) byIdMap.set(el.id, el); } },
    addEventListener: () => {},
  };
  return { doc, byIdMap, makeElement };
}

/**
 * Builds a fake click event whose `currentTarget` is the dropdown's
 * registered (fake) trigger element -- mirrors what the browser gives
 * statusDropdownToggleMenu for a real click.
 */
function fakeClick(trigger) {
  return { preventDefault() {}, stopPropagation() {}, currentTarget: trigger };
}

/**
 * Builds the Tasks toolbar's PR-status dropdown wiring
 * (ttPrStatusDropdownConfig, renderTtPrStatusFilter, ttSetPrStatusFilter)
 * fused with the real shared Status dropdown component, plus injectable
 * collaborator stubs. `renderTasksTab` re-invokes `renderTtPrStatusFilter`,
 * mirroring how the real `renderTasksTab` calls it on every poll -- the
 * exact re-render that used to blow away an open native `<select>`.
 */
export function makeTasksPrStatusFilter({ prStatus = "any" } = {}) {
  const { doc, byIdMap, makeElement } = makeFakeDocument();
  const container = makeElement();
  container.id = "tt-pr-filter";
  byIdMap.set(container.id, container);
  const taskTabFilters = { prStatus };
  const calls = { renderTasksTab: 0, ttScrollSelectionIntoView: 0, syncHash: 0 };
  let api;
  const deps = {
    document: doc,
    byId: (id) => byIdMap.get(id) || null,
    esc: (s) => String(s).replace(/&/g, "&amp;").replace(/</g, "&lt;").replace(/>/g, "&gt;").replace(/"/g, "&quot;"),
    cvar: (name) => `var(${name})`,
    window: { innerWidth: 1024, innerHeight: 768 },
    taskTabFilters,
    TT_PR_CI_COLORS,
    renderTasksTab: () => { calls.renderTasksTab++; api.renderTtPrStatusFilter(); },
    ttScrollSelectionIntoView: () => { calls.ttScrollSelectionIntoView++; },
    syncHash: () => { calls.syncHash++; },
  };
  // eslint-disable-next-line no-new-func -- evaluating the real shipped source is the point; see the header.
  const factory = new Function(
    "deps",
    `const { document, byId, esc, cvar, window, renderTasksTab, ttScrollSelectionIntoView, syncHash } = deps;
     var taskTabFilters = deps.taskTabFilters;
     var TT_PR_CI_COLORS = deps.TT_PR_CI_COLORS;
     ${sliceRegion(STATUS_DROPDOWN_REGION)}
     ${sliceRegion(REGIONS.tasks)}
     return {
       ttPrStatusDropdownConfig, renderTtPrStatusFilter, ttSetPrStatusFilter,
       statusDropdownToggleMenu, statusDropdownCloseAll, statusDropdownMenuKeydown,
       statusDropdownSelectSingle, statusDropdownMenuId, statusDropdownTriggerId,
       statusDropdownRegistry,
     };`,
  );
  api = factory(deps);
  const trigger = makeElement();
  trigger.id = api.statusDropdownTriggerId("tasks-pr");
  byIdMap.set(trigger.id, trigger);
  return { ...api, calls, container, trigger, taskTabFilters, byIdMap, makeElement, fakeClick: () => fakeClick(trigger) };
}

/**
 * Builds the Reviews sidebar's PR-status dropdown wiring
 * (reviewPrStatusDropdownConfig, renderReviewPrStatusFilter,
 * setReviewPrStatusFilter) fused with the real shared Status dropdown
 * component, plus injectable collaborator stubs. `renderReviews` re-invokes
 * `renderReviewPrStatusFilter`, mirroring how the real `renderReviews` calls
 * it on every poll.
 */
export function makeReviewsPrStatusFilter({ prStatus = "any" } = {}) {
  const { doc, byIdMap, makeElement } = makeFakeDocument();
  const container = makeElement();
  container.id = "review-pr-filter";
  byIdMap.set(container.id, container);
  const reviewFilters = { prStatus };
  const calls = { renderReviews: 0, syncHash: 0 };
  let api;
  const deps = {
    document: doc,
    byId: (id) => byIdMap.get(id) || null,
    esc: (s) => String(s).replace(/&/g, "&amp;").replace(/</g, "&lt;").replace(/>/g, "&gt;").replace(/"/g, "&quot;"),
    cvar: (name) => `var(${name})`,
    window: { innerWidth: 1024, innerHeight: 768 },
    reviewFilters,
    TT_PR_CI_COLORS,
    renderReviews: () => { calls.renderReviews++; api.renderReviewPrStatusFilter(); },
    syncHash: () => { calls.syncHash++; },
  };
  // eslint-disable-next-line no-new-func -- evaluating the real shipped source is the point; see the header.
  const factory = new Function(
    "deps",
    `const { document, byId, esc, cvar, window, renderReviews, syncHash } = deps;
     var reviewFilters = deps.reviewFilters;
     var TT_PR_CI_COLORS = deps.TT_PR_CI_COLORS;
     ${sliceRegion(STATUS_DROPDOWN_REGION)}
     ${sliceRegion(REGIONS.reviews)}
     return {
       reviewPrStatusDropdownConfig, renderReviewPrStatusFilter, setReviewPrStatusFilter,
       statusDropdownToggleMenu, statusDropdownCloseAll, statusDropdownMenuKeydown,
       statusDropdownSelectSingle, statusDropdownMenuId, statusDropdownTriggerId,
       statusDropdownRegistry,
     };`,
  );
  api = factory(deps);
  const trigger = makeElement();
  trigger.id = api.statusDropdownTriggerId("reviews-pr");
  byIdMap.set(trigger.id, trigger);
  return { ...api, calls, container, trigger, reviewFilters, byIdMap, makeElement, fakeClick: () => fakeClick(trigger) };
}
