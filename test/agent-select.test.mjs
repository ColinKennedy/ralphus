// Coverage for RAL-444 "Fix resolver-agent edit selector state", generalized
// in RAL-466 "Unify agent-selector combo box across Review Settings and
// Squad edit":
//
// The Edit Details resolver-agent <select> used to render its option list
// via `resolverOptionHtml`, which -- on a cache miss for the review's `cwd`
// -- kicked off a background `GET /api/agents` fetch AND immediately painted
// a hardcoded 3-item fallback list (claude / claude-code / codex-cli). If the
// review's real `resolver_agent` wasn't one of those three, no <option> ever
// got `selected`, so the browser defaulted the box to "claude" -- silently
// misrepresenting the review's actual configured agent until the modal was
// closed and reopened after the background fetch happened to land.
//
// The fix decouples "show the correct current value" from "show the
// complete option list": `agentSelectOptionHtml` now renders synchronously
// from only what's already known -- the cached list if one exists for this
// `cwd`, otherwise exactly one <option> (the current selected value, or a
// generic "agent default" placeholder when unset) -- so there is nothing
// else for the browser to fall back to. The complete list loads lazily, the
// moment the user actually opens the dropdown (`onAgentSelectMouseDown`),
// via `ensureAgentOptionsLoaded`, which dedups concurrent fetches per `cwd`
// and is bounded by a 5s timeout to a hardcoded fallback (now six agents,
// mirroring `BUILTIN_AGENTS` in daemon/src/agent_access.rs) so a stalled
// request can't hang the dropdown forever.
//
// RAL-466 lifted this machinery out from under the review-detail-only name
// (`RALPHUS-RESOLVER-AGENT-SELECT`) into a generic one
// (`RALPHUS-AGENT-SELECT`) so the same `<select>` backs the review
// resolver-agent field, the Project Review Settings resolver-agent field,
// and the Squad cell-edit agent field -- one fetch/cache/fallback/lazy-load
// implementation instead of three divergent copies.
//
// Run with `npm test` (node --test). See ./board-agent-select.mjs for how
// the functions are loaded out of the real board chunks.

import test from "node:test";
import assert from "node:assert/strict";
import { boardScript } from "./board-source.mjs";
import { makeAgentSelect } from "./board-agent-select.mjs";

const originalFetch = globalThis.fetch;
test.afterEach(() => {
  globalThis.fetch = originalFetch;
});

const CLAUDE = { id: "claude", kind: "builtin", backend: "claude" };
const CLAUDE_CODE = { id: "claude-code", kind: "builtin", backend: "claude-code" };
const CODEX = { id: "codex", kind: "builtin", backend: "codex" };
const PI = { id: "pi", kind: "builtin", backend: "pi" };
const OLLAMA = { id: "ollama", kind: "builtin", backend: "ollama" };
const ANTHROPIC = { id: "anthropic", kind: "builtin", backend: "anthropic" };
const CUSTOM_PROFILE = { id: "my-custom-profile", kind: "profile", backend: "claude" };

/** Waits for every currently-queued microtask (promise chain) to drain -- `setImmediate` runs after them, not just one hop's worth like a bare `await Promise.resolve()`. */
function flushMicrotasks() {
  return new Promise((resolve) => setImmediate(resolve));
}

// ---------- agentSelectOptionHtml: the field's initial (unexpanded) render ----------

test("initial render with a non-claude agent and nothing cached yet: shows exactly that agent, never defaults to claude", () => {
  const { agentSelectOptionHtml } = makeAgentSelect();
  const html = agentSelectOptionHtml("/repo/a", "codex");
  assert.equal((html.match(/<option/g) || []).length, 1, "must render exactly one option -- nothing else for the browser to fall back to");
  assert.match(html, /<option value="codex" selected>codex<\/option>/);
});

test("initial render with a custom profile agent not in the hardcoded fallback set: shows that exact value, not claude", () => {
  const { agentSelectOptionHtml } = makeAgentSelect();
  const html = agentSelectOptionHtml("/repo/a", "my-custom-profile");
  assert.match(html, /<option value="my-custom-profile" selected>my-custom-profile<\/option>/);
  assert.doesNotMatch(html, /claude/, "must not silently paint an unrelated fallback list containing claude");
});

test("initial render when unset (inherit default) and nothing cached yet: a generic placeholder, not a guessed concrete agent", () => {
  const { agentSelectOptionHtml } = makeAgentSelect();
  const html = agentSelectOptionHtml("/repo/a", "");
  assert.equal((html.match(/<option/g) || []).length, 1);
  assert.match(html, /<option value="" selected>agent default<\/option>/);
});

test("initial render with the list already cached for this cwd (e.g. a prior dropdown open, or another review sharing the cwd): the full list renders immediately, correctly selected", () => {
  const cache = new Map([["/repo/a", { agents: [CLAUDE, CLAUDE_CODE, CODEX, PI], defaultAgent: "claude" }]]);
  const { agentSelectOptionHtml } = makeAgentSelect({ agentOptionsByCwd: cache });
  const html = agentSelectOptionHtml("/repo/a", "codex");
  assert.equal((html.match(/<option/g) || []).length, 4, "the complete cached list, not a fallback guess");
  assert.match(html, /<option value="codex" selected>codex<\/option>/);
});

test("repeated close-and-reopen renders (repeated calls against the same warm cache) are identical", () => {
  const cache = new Map([["/repo/a", { agents: [CLAUDE, CODEX, PI], defaultAgent: "claude" }]]);
  const { agentSelectOptionHtml } = makeAgentSelect({ agentOptionsByCwd: cache });
  const first = agentSelectOptionHtml("/repo/a", "pi");
  const second = agentSelectOptionHtml("/repo/a", "pi");
  assert.equal(first, second);
  assert.match(second, /<option value="pi" selected>pi<\/option>/);
});

// ---------- agentSelectOptionHtmlFromEntry: full-list rendering ----------

test("marks the effective default agent's label and preselects it when selected is empty (unset)", () => {
  const { agentSelectOptionHtmlFromEntry } = makeAgentSelect();
  const html = agentSelectOptionHtmlFromEntry({ agents: [CLAUDE, CLAUDE_CODE, CODEX], defaultAgent: "claude-code" }, "");
  assert.match(html, /<option value="claude-code" selected>claude-code \(default\)<\/option>/);
  assert.doesNotMatch(html, /value="claude" selected/);
});

test("a profile-kind agent gets a profile suffix naming its backend", () => {
  const { agentSelectOptionHtmlFromEntry } = makeAgentSelect();
  const html = agentSelectOptionHtmlFromEntry({ agents: [CUSTOM_PROFILE], defaultAgent: "claude" }, "my-custom-profile");
  assert.match(html, /my-custom-profile \(profile → claude\)/);
});

test("options are sorted alphabetically by id", () => {
  const { agentSelectOptionHtmlFromEntry } = makeAgentSelect();
  const html = agentSelectOptionHtmlFromEntry({ agents: [PI, CLAUDE, CODEX], defaultAgent: "claude" }, "claude");
  const ids = [...html.matchAll(/<option value="([^"]+)"/g)].map((m) => m[1]);
  assert.deepEqual(ids, ["claude", "codex", "pi"]);
});

// ---------- ensureAgentOptionsLoaded: click-triggered loading ----------

test("a cached entry resolves immediately with no network fetch", async () => {
  globalThis.fetch = () => { throw new Error("must not fetch when already cached"); };
  const cache = new Map([["/repo/a", { agents: [CLAUDE, CODEX], defaultAgent: "claude" }]]);
  const { ensureAgentOptionsLoaded } = makeAgentSelect({ agentOptionsByCwd: cache });
  const entry = await ensureAgentOptionsLoaded("/repo/a");
  assert.deepEqual(entry, { agents: [CLAUDE, CODEX], defaultAgent: "claude" });
});

test("concurrent opens for the same cwd share one in-flight fetch instead of firing a duplicate", async () => {
  let fetchCalls = 0;
  globalThis.fetch = async () => {
    fetchCalls++;
    await Promise.resolve();
    return { json: async () => ({ agents: [CLAUDE, CODEX], default_agent: "claude" }) };
  };
  const { ensureAgentOptionsLoaded } = makeAgentSelect();
  const [a, b] = await Promise.all([ensureAgentOptionsLoaded("/repo/a"), ensureAgentOptionsLoaded("/repo/a")]);
  assert.equal(fetchCalls, 1, "the second concurrent open must reuse the first's in-flight fetch");
  assert.deepEqual(a, { agents: [CLAUDE, CODEX], defaultAgent: "claude" });
  assert.deepEqual(b, a);
});

test("a fetch failure resolves to the hardcoded fallback list, which mirrors BUILTIN_AGENTS (claude, claude-code, codex, pi, ollama, anthropic)", async () => {
  globalThis.fetch = async () => { throw new Error("network error"); };
  const { ensureAgentOptionsLoaded, AGENT_SELECT_FALLBACK_AGENTS } = makeAgentSelect();
  const entry = await ensureAgentOptionsLoaded("/repo/a");
  assert.deepEqual(entry.agents, AGENT_SELECT_FALLBACK_AGENTS);
  assert.deepEqual(
    entry.agents.map((a) => a.id).sort(),
    ["anthropic", "claude", "claude-code", "codex", "ollama", "pi"],
    "the fallback list must mirror BUILTIN_AGENTS in daemon/src/agent_access.rs exactly",
  );
  assert.equal(entry.agents.length, 6);
});

test("an empty agents list from the server is treated the same as a failure (falls back, not an empty dropdown)", async () => {
  globalThis.fetch = async () => ({ json: async () => ({ agents: [], default_agent: "ollama" }) });
  const { ensureAgentOptionsLoaded, AGENT_SELECT_FALLBACK_AGENTS } = makeAgentSelect();
  const entry = await ensureAgentOptionsLoaded("/repo/a");
  assert.deepEqual(entry.agents, AGENT_SELECT_FALLBACK_AGENTS);
});

test("a stalled fetch times out to the fallback list rather than hanging the dropdown forever", async (t) => {
  t.mock.timers.enable({ apis: ["setTimeout"] });
  globalThis.fetch = () => new Promise(() => {}); // never resolves
  const { ensureAgentOptionsLoaded, AGENT_SELECT_FALLBACK_AGENTS, AGENT_SELECT_LOAD_TIMEOUT_MS } = makeAgentSelect();
  const pending = ensureAgentOptionsLoaded("/repo/a");
  t.mock.timers.tick(AGENT_SELECT_LOAD_TIMEOUT_MS);
  const entry = await pending;
  assert.deepEqual(entry.agents, AGENT_SELECT_FALLBACK_AGENTS);
});

test("a successful fetch caches the result and refreshes the review-detail pane's read-only resolver label", async () => {
  globalThis.fetch = async () => ({ json: async () => ({ agents: [CLAUDE, CODEX], default_agent: "codex" }) });
  const h = makeAgentSelect();
  await h.ensureAgentOptionsLoaded("/repo/a");
  assert.deepEqual(h.agentOptionsByCwd.get("/repo/a"), { agents: [CLAUDE, CODEX], defaultAgent: "codex" });
  assert.equal(h.renderReviewDetailCalls, 1);
});

// ---------- onAgentSelectMouseDown: the actual dropdown-open handler ----------

/** A minimal fake <select>: just enough state for the handler to read/mutate. */
function fakeSelect(value) {
  return { value, innerHTML: "", showPicker: null, focusCalls: 0, focus() { this.focusCalls++; } };
}
function fakeEvent() {
  return { preventedDefault: false, preventDefault() { this.preventedDefault = true; } };
}

test("a cached list is spliced in synchronously, and the native dropdown is left alone (no preventDefault)", () => {
  const cache = new Map([["/repo/a", { agents: [CLAUDE, CODEX, PI], defaultAgent: "claude" }]]);
  const { onAgentSelectMouseDown } = makeAgentSelect({ agentOptionsByCwd: cache });
  const select = fakeSelect("codex");
  const e = fakeEvent();
  onAgentSelectMouseDown(e, select, "/repo/a");
  assert.equal(e.preventedDefault, false);
  assert.equal((select.innerHTML.match(/<option/g) || []).length, 3);
  assert.match(select.innerHTML, /<option value="codex" selected>codex<\/option>/);
});

test("delayed agent data: opening the dropdown before the fetch resolves suppresses the native popup and waits, rather than showing a stale/guessed list", async () => {
  let resolveFetch;
  globalThis.fetch = () => new Promise((resolve) => { resolveFetch = resolve; });
  const { onAgentSelectMouseDown } = makeAgentSelect();
  const select = fakeSelect("codex");
  select.showPicker = function () { this.showPickerCalls = (this.showPickerCalls || 0) + 1; };
  const e = fakeEvent();

  onAgentSelectMouseDown(e, select, "/repo/a");
  assert.equal(e.preventedDefault, true, "the native popup must not open on the still-incomplete single-option box");
  assert.equal(select.innerHTML, "", "nothing painted yet -- still waiting on the real list");

  resolveFetch({ json: async () => ({ agents: [CLAUDE, CODEX, PI], default_agent: "claude" }) });
  await flushMicrotasks();

  assert.equal((select.innerHTML.match(/<option/g) || []).length, 3, "the complete list must land once the fetch resolves");
  assert.match(select.innerHTML, /<option value="codex" selected>codex<\/option>/);
  assert.equal(select.showPickerCalls, 1, "the suppressed popup must be reopened once the real options are in place");
});

test("a second dropdown open for the same cwd while the first is still loading reuses that fetch instead of firing a duplicate", async () => {
  let fetchCalls = 0;
  let resolveFetch;
  globalThis.fetch = () => { fetchCalls++; return new Promise((resolve) => { resolveFetch = resolve; }); };
  const { onAgentSelectMouseDown } = makeAgentSelect();
  const selectA = fakeSelect("codex");
  const selectB = fakeSelect("codex");

  onAgentSelectMouseDown(fakeEvent(), selectA, "/repo/a");
  onAgentSelectMouseDown(fakeEvent(), selectB, "/repo/a");
  assert.equal(fetchCalls, 1);

  resolveFetch({ json: async () => ({ agents: [CLAUDE, CODEX], default_agent: "claude" }) });
  await flushMicrotasks();

  assert.equal((selectA.innerHTML.match(/<option/g) || []).length, 2);
  assert.equal((selectB.innerHTML.match(/<option/g) || []).length, 2);
});

// ---------- renderAgentSelectHtml: the shared <select> markup ----------

test("renderAgentSelectHtml wires up onAgentSelectMouseDown and the caller's onchange handler by name", () => {
  const { renderAgentSelectHtml } = makeAgentSelect();
  const html = renderAgentSelectHtml("e-agent", "/repo/a", "codex", "onCellEditAgentChange");
  assert.match(html, /<select id="e-agent"/);
  assert.match(html, /onchange="onCellEditAgentChange\(this\.value\)"/);
  assert.ok(html.includes(`onmousedown="onAgentSelectMouseDown(event,this,${JSON.stringify("/repo/a")})"`));
});

// ---------- board.html/chunk wiring: every consumer renders through the shared select ----------

test("the review-detail edit modal's resolver-agent field renders through the shared renderAgentSelectHtml, not a bespoke <select>", () => {
  const boardSource = boardScript();
  const fnStart = boardSource.indexOf("function renderResolverFieldsHtml(");
  const fnBody = boardSource.slice(fnStart, boardSource.indexOf("\n      }\n", fnStart));
  assert.match(fnBody, /renderAgentSelectHtml\(\s*"",\s*cwd,\s*resolverAgent,\s*onAgentChange/, "renderResolverFieldsHtml's shape changed -- update this wiring assertion alongside it");
});

test("the Project Review Settings modal preloads the shared agent cache so the resolver dropdown doesn't stay stuck on the placeholder", () => {
  const boardSource = boardScript();
  const fnStart = boardSource.indexOf("function openProjectReviewSettings(");
  const fnBody = boardSource.slice(fnStart, boardSource.indexOf("\n      }\n", fnStart));
  assert.match(fnBody, /preloadAgentSelect\(/, "openProjectReviewSettings must eagerly warm the agent-options cache for this project's cwd");
});

test("the Squad cell-edit form's agent field renders through the shared renderAgentSelectHtml, not a free-text input", () => {
  const boardSource = boardScript();
  const fnStart = boardSource.indexOf("function editForm(");
  const fnBody = boardSource.slice(fnStart, boardSource.indexOf("\n      async function saveEdit(", fnStart));
  assert.match(fnBody, /renderAgentSelectHtml\(\s*"e-agent"/, "the Squad cell-edit agent field must use the shared combo box");
  assert.doesNotMatch(fnBody, /<input[^>]*list="[^"]*agent[^"]*"/i, "must not still be a free-text <input list> / datalist field");
});

test("the New Task Simple tab's agent field renders through the shared renderAgentSelectHtml, not its own /api/agents/catalog-backed <select>", () => {
  const boardSource = boardScript();
  const fnStart = boardSource.indexOf("function ntSimpleTabHtml(");
  const fnBody = boardSource.slice(fnStart, boardSource.indexOf("\n      }\n", fnStart));
  assert.match(fnBody, /renderAgentSelectHtml\(\s*"nt-agent"/, "the New Task Simple tab's agent field must use the shared combo box");
  assert.doesNotMatch(boardSource, /fetch\(\s*["']\/api\/agents\/catalog/, "the New Task modal must not still fetch the standalone agent catalog endpoint");
});
