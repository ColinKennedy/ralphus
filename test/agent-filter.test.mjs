// RAL-486: coverage for the agent filters the Tasks and Reviews sidebars share.
//
// Both tabs now offer one compact dropdown instead of a row of loose per-agent
// checkboxes, and both drive it through the same pure core: which agents a task
// counts as using, the option list a tab offers, whether a squad survives a
// selection, the collapsed button's "shown/available" badge, and the
// auto-include-new-agents default. The behaviours that are easy to break while
// consolidating the UI — a task-level agent shadowing its cells' real ones, a
// newly-seen agent being silently hidden, an unnarrowed filter still drawing a
// badge — are what the assertions below pin down.
//
// Run with `npm test` (node --test). See ./board-agent-filter.mjs for how the
// logic is sliced out of the real board.html.

import test from "node:test";
import assert from "node:assert/strict";
import { agentFilter, boardSource } from "./board-agent-filter.mjs";

const {
  taskAgentNames,
  squadAgentNames,
  agentFilterOptions,
  squadMatchesAgentFilter,
  agentFilterBadge,
  syncedAgentSelection,
} = agentFilter;

/**
 * A minimal TaskView carrying only what the agent filter reads.
 * @param {string|null} agent raw task-level agent
 * @param {string[]} cellAgents one resolved agent per cell
 * @returns {object} task
 */
function task(agent, cellAgents) {
  return { agent, cells: cellAgents.map((a) => ({ agent: a })) };
}

/**
 * A minimal SquadView wrapping the given tasks.
 * @param {object[]} tasks
 * @returns {object} squad
 */
function squad(tasks) {
  return { id: "squad-1", tasks };
}

test("a task's agents are its cells' resolved agents", () => {
  assert.deepEqual(taskAgentNames(task(null, ["claude-code"])), ["claude-code"]);
  assert.deepEqual(taskAgentNames(task(null, ["claude-code", "ollama"])), ["claude-code", "ollama"]);
});

test("cell agents win over the raw task-level agent, which is only a default they inherit", () => {
  // The TOML said agent = "claude-code" at task level and one cell overrode it.
  // Filtering on the raw value alone would both claim an agent the task no
  // longer uses and miss the one it does.
  assert.deepEqual(taskAgentNames(task("claude-code", ["ollama"])), ["ollama"]);
  assert.deepEqual(taskAgentNames(task("claude-code", ["claude-code", "codex"])), ["claude-code", "codex"]);
});

test("a task with no cells falls back to its own task-level agent", () => {
  assert.deepEqual(taskAgentNames(task("codex", [])), ["codex"]);
});

test("a task with neither cells nor a task-level agent attributes to no agent", () => {
  assert.deepEqual(taskAgentNames(task(null, [])), []);
  assert.deepEqual(taskAgentNames({}), []);
});

test("duplicate cell agents collapse to one name", () => {
  assert.deepEqual(taskAgentNames(task(null, ["ollama", "ollama", "ollama"])), ["ollama"]);
});

test("a squad's agents span every one of its tasks", () => {
  const r = squad([task(null, ["ollama"]), task("codex", []), task(null, ["ollama", "claude-code"])]);
  assert.deepEqual(squadAgentNames(r).sort(), ["claude-code", "codex", "ollama"]);
});

test("options are deduplicated, sorted, and drop blanks", () => {
  assert.deepEqual(agentFilterOptions(["ollama", "codex", "ollama", "claude-code"]), ["claude-code", "codex", "ollama"]);
  assert.deepEqual(agentFilterOptions(["", "ollama", ""]), ["ollama"]);
  assert.deepEqual(agentFilterOptions([]), []);
});

test("a squad survives when any of its tasks uses a selected agent", () => {
  const r = squad([task(null, ["ollama"]), task(null, ["claude-code"])]);
  assert.equal(squadMatchesAgentFilter(r, new Set(["ollama", "claude-code"])), true);
  assert.equal(squadMatchesAgentFilter(r, new Set(["claude-code"])), true);
  assert.equal(squadMatchesAgentFilter(r, new Set(["codex"])), false);
  assert.equal(squadMatchesAgentFilter(r, new Set()), false);
});

test("a squad whose agent cannot be named is never filtered out", () => {
  // "none" can only clear away work the filter can actually name; hiding a
  // squad no checkbox in the menu corresponds to would be unexplainable.
  const r = squad([task(null, [])]);
  assert.equal(squadMatchesAgentFilter(r, new Set()), true);
  assert.equal(squadMatchesAgentFilter(r, new Set(["ollama"])), true);
  assert.equal(squadMatchesAgentFilter(squad([]), new Set()), true);
});

test("the collapsed badge shows a narrowed selection and nothing otherwise", () => {
  assert.equal(agentFilterBadge(2, 5), "2/5");
  assert.equal(agentFilterBadge(0, 5), "0/5");
  assert.equal(agentFilterBadge(5, 5), "");
  assert.equal(agentFilterBadge(0, 0), "");
});

test("a newly-discovered agent is filtered in until the user chooses explicitly", () => {
  const selected = new Set(["ollama"]);
  const next = syncedAgentSelection(false, selected, ["claude-code", "ollama"]);
  assert.deepEqual([...next].sort(), ["claude-code", "ollama"]);
});

test("an explicit selection is kept as-is, even as new agents appear", () => {
  const selected = new Set(["ollama"]);
  const next = syncedAgentSelection(true, selected, ["claude-code", "codex", "ollama"]);
  assert.equal(next, selected);
  assert.deepEqual([...next], ["ollama"]);
});

test("an explicitly emptied selection is not re-filled by the default sync", () => {
  // Clicking "none" is a real choice; re-syncing it back to everything would
  // make the chip look broken.
  const next = syncedAgentSelection(true, new Set(), ["ollama"]);
  assert.deepEqual([...next], []);
});

test("both tabs render the shared dropdown and no loose agent checkbox rows remain", () => {
  assert.match(boardSource, /id="task-agent-filter"/);
  assert.match(boardSource, /id="review-agent-filter"/);
  assert.doesNotMatch(boardSource, /review-resolver-filters/);
  // One dropdown per tab, both going through the same builder.
  assert.equal((boardSource.match(/data-click="toggleAgentFilterMenu"/g) || []).length, 1);
  assert.match(boardSource, /function agentFilterHtml\(/);
});
