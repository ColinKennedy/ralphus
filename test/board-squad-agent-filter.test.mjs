// Coverage for the Squads sidebar's Agent dropdown (RAL-486 follow-up): its
// option list, the auto-sync-until-explicitly-chosen default, the toggle/
// all/none wiring, and the `visibleSquads` predicate it drives. See
// ./board-squad-agent-filter.mjs for how this is sliced out of the real
// board chunks.
//
// Run with `npm test` (node --test).

import test from "node:test";
import assert from "node:assert/strict";
import { makeSquadAgentFilter, squad, task } from "./board-squad-agent-filter.mjs";

const TWO_SQUADS = [
  squad("squad-1", [task(null, ["claude-code"]), task(null, ["codex"])]),
  squad("squad-2", [task(null, ["ollama"])]),
];

// ---------- option list ----------

test("squadAgents unions the resolved agents of a squad's tasks, deduped and sorted", () => {
  const { squadAgents } = makeSquadAgentFilter();
  const s = squad("squad-1", [task(null, ["codex", "claude-code"]), task(null, ["codex"])]);
  assert.deepEqual(squadAgents(s), ["claude-code", "codex"]);
});

test("squadAgents falls back to a cell-less task's own agent", () => {
  const { squadAgents } = makeSquadAgentFilter();
  assert.deepEqual(squadAgents(squad("squad-1", [task("ollama", [])])), ["ollama"]);
});

test("squadAgents yields nothing for a squad whose tasks resolve no agent at all", () => {
  const { squadAgents } = makeSquadAgentFilter();
  assert.deepEqual(squadAgents(squad("squad-1", [task(null, [])])), []);
});

test("squadAgentOptions unions every loaded squad's agents, deduped and sorted", () => {
  const { squadAgentOptions } = makeSquadAgentFilter({ squads: TWO_SQUADS });
  assert.deepEqual(squadAgentOptions(), ["claude-code", "codex", "ollama"]);
});

// ---------- auto-sync default ----------

test("syncSquadAgentDefault filters in every agent currently in use until the user picks", () => {
  const { syncSquadAgentDefault, filters } = makeSquadAgentFilter({ squads: TWO_SQUADS });
  syncSquadAgentDefault();
  assert.deepEqual([...filters.agents].sort(), ["claude-code", "codex", "ollama"]);
});

test("syncSquadAgentDefault leaves a user-chosen selection alone -- a newly-seen agent stays filtered out", () => {
  const { syncSquadAgentDefault, filters } = makeSquadAgentFilter({
    squads: TWO_SQUADS,
    agents: new Set(["codex"]),
    defaulted: true,
  });
  syncSquadAgentDefault();
  assert.deepEqual([...filters.agents], ["codex"]);
});

// ---------- dropdown config ----------

test("squadAgentDropdownConfig is a dotless multi-select over the in-use agents, with agent-noun copy", () => {
  const { squadAgentDropdownConfig } = makeSquadAgentFilter({ squads: TWO_SQUADS, agents: new Set(["codex"]), defaulted: true });
  const config = squadAgentDropdownConfig();
  assert.equal(config.id, "squads-agent");
  assert.equal(config.label, "Agent");
  assert.equal(config.mode, "multi");
  assert.equal(config.itemNoun, "agent");
  assert.deepEqual(config.options, [
    { value: "claude-code", label: "claude-code" },
    { value: "codex", label: "codex" },
    { value: "ollama", label: "ollama" },
  ]);
  // RAL-486: an agent has no lifecycle color, so its option renders dotless.
  assert.ok(config.options.every((o) => o.color === undefined));
  assert.deepEqual([...config.selected], ["codex"]);
  assert.match(config.optionTip("codex"), /codex/);
});

test("renderSquadAgentFilter paints the sidebar's agent-filter container", () => {
  const { renderSquadAgentFilter, container } = makeSquadAgentFilter({ squads: TWO_SQUADS });
  renderSquadAgentFilter();
  assert.match(container.innerHTML, /Agent/);
  assert.match(container.innerHTML, /status-dropdown-trigger-squads-agent/);
});

// ---------- toggle / all / none ----------

test("toggleSquadAgent adds and removes one agent, re-rendering and syncing the hash each time", () => {
  const { toggleSquadAgent, filters, calls, isDefaulted } = makeSquadAgentFilter({
    squads: TWO_SQUADS,
    agents: new Set(["claude-code", "codex", "ollama"]),
  });
  toggleSquadAgent("codex", false);
  assert.deepEqual([...filters.agents].sort(), ["claude-code", "ollama"]);
  assert.equal(calls.renderSquads, 1);
  assert.equal(calls.syncHash, 1);
  toggleSquadAgent("codex", true);
  assert.deepEqual([...filters.agents].sort(), ["claude-code", "codex", "ollama"]);
  assert.equal(calls.renderSquads, 2);
  // Once the user has touched it, the selection is no longer auto-synced.
  assert.equal(isDefaulted(), true);
});

test("allSquadAgent(true) selects every in-use agent and allSquadAgent(false) empties the filter", () => {
  const { allSquadAgent, filters, calls } = makeSquadAgentFilter({ squads: TWO_SQUADS });
  allSquadAgent(false);
  assert.deepEqual([...filters.agents], []);
  allSquadAgent(true);
  assert.deepEqual([...filters.agents].sort(), ["claude-code", "codex", "ollama"]);
  assert.equal(calls.renderSquads, 2);
  assert.equal(calls.syncHash, 2);
});

// ---------- visibleSquads ----------

test("visibleSquads shows every squad before the user narrows the agent filter", () => {
  const { visibleSquads } = makeSquadAgentFilter({ squads: TWO_SQUADS });
  assert.deepEqual(visibleSquads().map((s) => s.id), ["squad-1", "squad-2"]);
});

test("visibleSquads keeps a squad when ANY of its tasks uses a selected agent", () => {
  const { visibleSquads } = makeSquadAgentFilter({
    squads: TWO_SQUADS,
    agents: new Set(["codex"]),
    defaulted: true,
  });
  // squad-1 has a codex task alongside a claude-code one; squad-2 has neither.
  assert.deepEqual(visibleSquads().map((s) => s.id), ["squad-1"]);
});

test("visibleSquads hides everything once the agent filter is explicitly emptied", () => {
  const { visibleSquads } = makeSquadAgentFilter({ squads: TWO_SQUADS, agents: new Set(), defaulted: true });
  assert.deepEqual(visibleSquads(), []);
});

test("visibleSquads hides a squad with no resolved agent at all once the filter is active", () => {
  const { visibleSquads } = makeSquadAgentFilter({
    squads: [squad("squad-1", [task(null, [])]), squad("squad-2", [task(null, ["codex"])])],
    agents: new Set(["codex"]),
    defaulted: true,
  });
  assert.deepEqual(visibleSquads().map((s) => s.id), ["squad-2"]);
});

test("visibleSquads still reveals a directly-navigated squad the agent filter would exclude", () => {
  // RAL-461's §reveal bypass must survive the new predicate: a "go to squad"
  // link can never land on a squad the sidebar then hides.
  const filter = makeSquadAgentFilter({ squads: TWO_SQUADS, agents: new Set(["codex"]), defaulted: true });
  filter.setRevealed("squad-2");
  assert.deepEqual(filter.visibleSquads().map((s) => s.id).sort(), ["squad-1", "squad-2"]);
});
