// Loads the shared agent-picker <select>'s option-building + lazy-load logic
// (RAL-444, generalized in RAL-466 to back every agent-picking <select> in
// the board UI -- the review resolver-agent field, the Project Review
// Settings resolver-agent field, and the Squad cell-edit agent field) out of
// the board chunk files (librarian/assets/board/) so it can be exercised
// under `node --test` with no browser and no build step.
//
// Same slice-the-real-source approach as ./board-linked-output-fetch.mjs --
// the region between the RALPHUS-AGENT-SELECT markers calls only `esc` and
// the ambient `fetch`/`setTimeout` globals (the latter two stubbed per-test
// via `globalThis.fetch` / node:test's mock timers) plus a shared
// `agentOptionsByCwd` cache and `renderReviewDetail` callback, both supplied
// as factory params below since they're declared in other chunk files in the
// real page.

import { boardScript } from "./board-source.mjs";

const BEGIN = "// RALPHUS-AGENT-SELECT:BEGIN";
const END = "// RALPHUS-AGENT-SELECT:END";

const html = boardScript();
const from = html.indexOf(BEGIN);
const to = html.indexOf(END);
if (from === -1 || to === -1 || to < from) {
  throw new Error(
    `board-agent-select: could not find the ${BEGIN} / ${END} markers in the board chunks. ` +
      "If this logic moved, move the markers with it -- these tests are its only coverage.",
  );
}
const source = html.slice(from + BEGIN.length, to);

// eslint-disable-next-line no-new-func -- evaluating the real shipped source is the point; see the header.
const factory = new Function(
  "esc",
  "agentOptionsByCwd",
  "renderReviewDetail",
  `${source}
   return {
     agentSelectOptionHtml,
     agentSelectOptionHtmlFromEntry,
     ensureAgentOptionsLoaded,
     onAgentSelectMouseDown,
     renderAgentSelectHtml,
     preloadAgentSelect,
     AGENT_SELECT_FALLBACK_AGENTS,
     AGENT_SELECT_FALLBACK_DEFAULT,
     AGENT_SELECT_LOAD_TIMEOUT_MS,
   };`,
);

const defaultEsc = (s) => String(s).replace(/&/g, "&amp;").replace(/</g, "&lt;").replace(/>/g, "&gt;").replace(/"/g, "&quot;");

/**
 * Builds the shared agent-select helpers, evaluated straight from the board
 * chunks, wired to a fresh `agentOptionsByCwd` cache (or one supplied by the
 * caller, to share state across calls the way the real page's page-lifetime
 * cache does) and a `renderReviewDetail` spy.
 * @param {{agentOptionsByCwd?: Map<string, {agents: object[], defaultAgent: string}>, renderReviewDetail?: () => void, esc?: (s: string) => string}} [deps]
 * @returns {ReturnType<typeof factory> & {agentOptionsByCwd: Map<string, object>, renderReviewDetailCalls: number}}
 */
export function makeAgentSelect(deps = {}) {
  const {
    agentOptionsByCwd = new Map(),
    esc = defaultEsc,
  } = deps;
  const calls = { renderReviewDetail: 0 };
  const renderReviewDetail = deps.renderReviewDetail || (() => { calls.renderReviewDetail++; });
  const api = factory(esc, agentOptionsByCwd, renderReviewDetail);
  return { ...api, agentOptionsByCwd, get renderReviewDetailCalls() { return calls.renderReviewDetail; } };
}
