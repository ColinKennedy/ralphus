// Loads the Squads sidebar's and Tasks toolbar's project-filter dropdown
// logic (RAL-345, RAL-439) straight out of librarian/assets/board/, sliced by
// the RALPHUS-PROJECT-FILTER-MENU / RALPHUS-TT-PROJECT-FILTER-MENU markers.
// Same slice-the-real-source approach as ./board-filter-selection-scroll.mjs
// -- this logic touches `document` directly (menu creation/removal), which
// is why it lives outside the RALPHUS-TT-FILTER-SELECTION-SCROLL region and
// needs its own DOM stubs here rather than reusing that harness.

import { boardScript } from "./board-source.mjs";
import { fileURLToPath } from "node:url";
import { dirname, join } from "node:path";

const repoRoot = join(dirname(fileURLToPath(import.meta.url)), "..");
export const boardPath = join(repoRoot, "librarian", "assets", "board");

const html = boardScript();

function sliceRegion([BEGIN, END]) {
  const from = html.indexOf(BEGIN);
  const to = html.indexOf(END);
  if (from === -1 || to === -1 || to < from) {
    throw new Error(
      `board-project-filter-menu: could not find the ${BEGIN} / ${END} markers in ${boardPath}. ` +
        "If this logic moved, move the markers with it -- these tests are its only coverage.",
    );
  }
  return html.slice(from + BEGIN.length, to);
}

const REGIONS = {
  squads: ["// RALPHUS-PROJECT-FILTER-MENU:BEGIN", "// RALPHUS-PROJECT-FILTER-MENU:END"],
  tasks: ["// RALPHUS-TT-PROJECT-FILTER-MENU:BEGIN", "// RALPHUS-TT-PROJECT-FILTER-MENU:END"],
};

/** A minimal fake element: just enough for getElementById/createElement/innerHTML/remove bookkeeping. */
function makeFakeDocument() {
  const byIdMap = new Map();
  const clickListeners = [];
  const doc = {
    getElementById: (id) => byIdMap.get(id) || null,
    createElement: () => ({ innerHTML: "", style: {}, className: "", id: "" }),
    body: {
      appendChild: (el) => {
        if (el.id) byIdMap.set(el.id, el);
      },
    },
    addEventListener: (type, fn) => { if (type === "click") clickListeners.push(fn); },
  };
  return { doc, byIdMap, clickListeners };
}

/**
 * Builds the Squads sidebar's project-filter functions
 * (toggleProjectFilter, clearProjectFilter, renderProjectFilterChips,
 * projectFilterMenuRowsHtml, openProjectFilterMenu, closeProjectFilterMenu)
 * with injectable collaborator stubs.
 */
export function makeSquadsProjectFilterMenu({
  filters = { projects: new Set() },
  projects = [{ name: "acme" }, { name: "beta" }],
} = {}) {
  const { doc, byIdMap, clickListeners } = makeFakeDocument();
  const elements = { "project-filter": { innerHTML: "" } };
  byIdMap.set("project-filter", elements["project-filter"]);
  const calls = { renderSquads: 0, syncHash: 0 };
  const deps = {
    document: doc,
    byId: (id) => elements[id] || byIdMap.get(id),
    esc: (s) => String(s).replace(/&/g, "&amp;").replace(/</g, "&lt;").replace(/>/g, "&gt;").replace(/"/g, "&quot;"),
    renderSquads: () => calls.renderSquads++,
    syncHash: () => calls.syncHash++,
    filters,
    projects,
  };
  // eslint-disable-next-line no-new-func -- evaluating the real shipped source is the point; see the header.
  const factory = new Function(
    "deps",
    `const { document, byId, esc, renderSquads, syncHash } = deps;
     var filters = deps.filters;
     var projects = deps.projects;
     var registeredProjectNames = projects.map((p) => p.name);
     ${sliceRegion(REGIONS.squads)}
     return { toggleProjectFilter, clearProjectFilter, renderProjectFilterChips, projectFilterMenuRowsHtml, openProjectFilterMenu, closeProjectFilterMenu };`,
  );
  const api = factory(deps);
  return { ...api, calls, elements, byIdMap, clickListeners, filters };
}

/**
 * Builds the Tasks toolbar's project-filter functions
 * (ttToggleProjectFilter, ttClearProjectFilter, renderTtProjectFilter,
 * ttProjectFilterMenuRowsHtml, ttOpenProjectFilterMenu, ttCloseProjectFilterMenu)
 * with injectable collaborator stubs.
 */
export function makeTasksProjectFilterMenu({
  taskTabFilters = { projects: new Set() },
  projects = [{ name: "acme" }, { name: "beta" }],
} = {}) {
  const { doc, byIdMap, clickListeners } = makeFakeDocument();
  const elements = { "tt-project-filter": { innerHTML: "" } };
  byIdMap.set("tt-project-filter", elements["tt-project-filter"]);
  const calls = { renderTasksTab: 0, syncHash: 0, ttScrollSelectionIntoView: 0, ttCloseColMenu: 0 };
  const deps = {
    document: doc,
    byId: (id) => elements[id] || byIdMap.get(id),
    esc: (s) => String(s).replace(/&/g, "&amp;").replace(/</g, "&lt;").replace(/>/g, "&gt;").replace(/"/g, "&quot;"),
    renderTasksTab: () => calls.renderTasksTab++,
    syncHash: () => calls.syncHash++,
    ttScrollSelectionIntoView: () => calls.ttScrollSelectionIntoView++,
    ttCloseColMenu: () => calls.ttCloseColMenu++,
    taskTabFilters,
    projects,
  };
  // eslint-disable-next-line no-new-func -- evaluating the real shipped source is the point; see the header.
  const factory = new Function(
    "deps",
    `const { document, byId, esc, renderTasksTab, syncHash, ttScrollSelectionIntoView, ttCloseColMenu } = deps;
     var taskTabFilters = deps.taskTabFilters;
     var projects = deps.projects;
     var registeredProjectNames = projects.map((p) => p.name);
     ${sliceRegion(REGIONS.tasks)}
     return { ttToggleProjectFilter, ttClearProjectFilter, renderTtProjectFilter, ttProjectFilterMenuRowsHtml, ttOpenProjectFilterMenu, ttCloseProjectFilterMenu };`,
  );
  const api = factory(deps);
  return { ...api, calls, elements, byIdMap, clickListeners, taskTabFilters };
}
