// Loads the Squads sidebar's Agent dropdown (RAL-486 follow-up) straight out
// of librarian/assets/board/, fused with the real shared Status dropdown
// component (RALPHUS-STATUS-DROPDOWN) and the real `visibleSquads` filter, so
// toggling an agent here drives the same code paths production does rather
// than a mock of them. Same slice-the-real-source approach as
// ./board-pr-status-filter.mjs.

import { boardScript } from "./board-source.mjs";

const STATUS_DROPDOWN_REGION = ["// RALPHUS-STATUS-DROPDOWN:BEGIN", "// RALPHUS-STATUS-DROPDOWN:END"];
const AGENT_FILTER_REGION = ["// RALPHUS-SQUAD-AGENT-FILTER:BEGIN", "// RALPHUS-SQUAD-AGENT-FILTER:END"];
const VISIBLE_SQUADS_REGION = ["// RALPHUS-VISIBLE-SQUADS:BEGIN", "// RALPHUS-VISIBLE-SQUADS:END"];

function sliceRegion([BEGIN, END]) {
  const html = boardScript();
  const from = html.indexOf(BEGIN);
  const to = html.indexOf(END);
  if (from === -1 || to === -1 || to < from) {
    throw new Error(
      `board-squad-agent-filter: could not find the ${BEGIN} / ${END} markers in librarian/assets/board/. ` +
        "If this logic moved, move the markers with it -- these tests are its only coverage.",
    );
  }
  return html.slice(from + BEGIN.length, to);
}

/** A minimal fake element/document -- same shape as ./board-pr-status-filter.mjs's. */
function makeFakeDocument() {
  const byIdMap = new Map();

  function makeElement() {
    const attrs = new Map();
    const el = {
      id: "",
      className: "",
      innerHTML: "",
      style: {},
      setAttribute(name, value) { attrs.set(name, String(value)); },
      getAttribute(name) { return attrs.has(name) ? attrs.get(name) : null; },
      focus() {},
      addEventListener() {},
      querySelector() { return makeElement(); },
      getBoundingClientRect() { return { left: 0, right: 0, top: 0, bottom: 0 }; },
      remove() { if (el.id) byIdMap.delete(el.id); },
    };
    return el;
  }

  const doc = {
    getElementById: (id) => byIdMap.get(id) || null,
    createElement: () => makeElement(),
    querySelectorAll: () => [],
    body: { appendChild: (el) => { if (el.id) byIdMap.set(el.id, el); } },
    addEventListener: () => {},
  };
  return { doc, byIdMap, makeElement };
}

/** A squad row shaped like the wire's SquadView, with just the fields these filters read. */
export function squad(id, tasks, extra = {}) {
  return {
    id,
    label: extra.label ?? id,
    state: extra.state ?? "running",
    created_at_ms: extra.created_at_ms ?? 0,
    projects: extra.projects ?? [],
    tasks,
  };
}

/** A task row carrying only what `ttTaskAgents` reads. */
export function task(agent, cellAgents) {
  return { agent, cells: (cellAgents || []).map((a) => ({ agent: a })) };
}

/**
 * Builds the Squads sidebar's Agent-filter wiring (squadAgents,
 * squadAgentOptions, syncSquadAgentDefault, squadAgentDropdownConfig,
 * renderSquadAgentFilter, toggleSquadAgent, allSquadAgent) fused with the
 * real shared Status dropdown component and the real `visibleSquads`, plus
 * injectable collaborator stubs. `renderSquads` re-invokes
 * `renderSquadAgentFilter`, mirroring how the real `renderSquads` calls it on
 * every poll.
 */
export function makeSquadAgentFilter({ squads = [], agents = new Set(), defaulted = false, hiddenSquadIds = new Set() } = {}) {
  const { doc, byIdMap, makeElement } = makeFakeDocument();
  const container = makeElement();
  container.id = "agent-filter";
  byIdMap.set(container.id, container);
  const filters = {
    q: "",
    sort: "date",
    dir: -1,
    status: new Set(["running", "done", "failed", "pending", "cancelled"]),
    showHidden: false,
    projects: new Set(),
    agents,
  };
  const calls = { renderSquads: 0, syncHash: 0 };
  let api;
  const deps = {
    document: doc,
    byId: (id) => byIdMap.get(id) || null,
    esc: (s) => String(s).replace(/&/g, "&amp;").replace(/</g, "&lt;").replace(/>/g, "&gt;").replace(/"/g, "&quot;"),
    cvar: (name) => `var(${name})`,
    window: { innerWidth: 1024, innerHeight: 768 },
    squads,
    filters,
    hiddenSquadIds,
    defaulted,
    // The one collaborator from another region: a task's resolved agents.
    ttTaskAgents: (t) => {
      const cells = t.cells || [];
      if (cells.length) return [...new Set(cells.map((c) => c.agent))];
      return t.agent ? [t.agent] : [];
    },
    renderSquads: () => { calls.renderSquads++; api.renderSquadAgentFilter(); },
    syncHash: () => { calls.syncHash++; },
  };
  // eslint-disable-next-line no-new-func -- evaluating the real shipped source is the point; see the header.
  const factory = new Function(
    "deps",
    `const { document, byId, esc, cvar, window, ttTaskAgents, renderSquads, syncHash } = deps;
     var squads = deps.squads;
     var filters = deps.filters;
     var hiddenSquadIds = deps.hiddenSquadIds;
     var squadAgentDefaulted = deps.defaulted;
     var revealedSquadId = null;
     ${sliceRegion(STATUS_DROPDOWN_REGION)}
     ${sliceRegion(AGENT_FILTER_REGION)}
     ${sliceRegion(VISIBLE_SQUADS_REGION)}
     return {
       squadAgents, squadAgentOptions, syncSquadAgentDefault, squadAgentDropdownConfig,
       renderSquadAgentFilter, toggleSquadAgent, allSquadAgent, visibleSquads,
       statusDropdownTriggerId, statusDropdownRegistry,
       isDefaulted: () => squadAgentDefaulted,
       setRevealed: (id) => { revealedSquadId = id; },
     };`,
  );
  api = factory(deps);
  return { ...api, calls, container, filters, byIdMap };
}
