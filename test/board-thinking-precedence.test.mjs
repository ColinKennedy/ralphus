// RAL-516: coverage for the "Show Thinking" checkbox's four-layer
// precedence chain. Layers 1-2 (backend capability, then a DB agent
// profile's override) decide whether the checkbox is rendered at all, and
// are covered on the Rust side by
// daemon/src/agent_profiles.rs::thinking_capable_for_agent's tests. This
// file covers layers 3-4 -- once a pane's checkbox is shown, whether it
// starts checked: the config-driven `hide_thinking` default, then a
// per-pane toggle override that always wins over that default.
//
// Run with `npm test` (node --test). See ./board-thinking-precedence.mjs
// for how peekShowsThinking/toggleShowThinking are loaded out of the real
// board.html.

import test from "node:test";
import assert from "node:assert/strict";
import { makeThinkingPrecedence } from "./board-thinking-precedence.mjs";

// ---- peekShowsThinking: layer 3 (hideThinkingDefault) ----

test("with no per-pane override, a false hideThinkingDefault shows thinking", () => {
  const { peekShowsThinking } = makeThinkingPrecedence({ hideThinkingDefault: false });
  assert.equal(peekShowsThinking("cell|squad-abc|0|1"), true);
});

test("with no per-pane override, a true hideThinkingDefault hides thinking", () => {
  const { peekShowsThinking } = makeThinkingPrecedence({ hideThinkingDefault: true });
  assert.equal(peekShowsThinking("cell|squad-abc|0|1"), false);
});

// ---- peekShowsThinking: layer 4 (per-pane override wins over layer 3) ----

test("a per-pane override of true wins over a hidden-by-default config", () => {
  const { peekShowsThinking } = makeThinkingPrecedence({
    hideThinkingDefault: true,
    peekShowThinking: { "cell|squad-abc|0|1": true },
  });
  assert.equal(peekShowsThinking("cell|squad-abc|0|1"), true);
});

test("a per-pane override of false wins over a shown-by-default config", () => {
  const { peekShowsThinking } = makeThinkingPrecedence({
    hideThinkingDefault: false,
    peekShowThinking: { "cell|squad-abc|0|1": false },
  });
  assert.equal(peekShowsThinking("cell|squad-abc|0|1"), false);
});

test("a per-pane override only applies to the pane it was set on", () => {
  const { peekShowsThinking } = makeThinkingPrecedence({
    hideThinkingDefault: false,
    peekShowThinking: { "cell|squad-abc|0|1": false },
  });
  assert.equal(peekShowsThinking("cell|squad-abc|0|2"), true);
});

// ---- toggleShowThinking: sets the layer-4 override and re-renders ----

test("toggling a pane whose tape is already loaded sets the override and re-renders", () => {
  const { toggleShowThinking, peekShowThinking, peekTape, calls } = makeThinkingPrecedence({
    peekTape: { "cell|squad-abc|0|1": ["some", "lines"] },
  });
  toggleShowThinking("cell|squad-abc|0|1", true);
  assert.equal(peekShowThinking["cell|squad-abc|0|1"], true);
  assert.deepEqual(calls.renderPeekTape, ["cell|squad-abc|0|1"]);
  assert.ok(peekTape["cell|squad-abc|0|1"]);
});

test("toggling a pane whose tape has not been loaded sets the override but does not render", () => {
  const { toggleShowThinking, peekShowThinking, calls } = makeThinkingPrecedence();
  toggleShowThinking("cell|squad-abc|0|1", false);
  assert.equal(peekShowThinking["cell|squad-abc|0|1"], false);
  assert.deepEqual(calls.renderPeekTape, []);
});

test("toggling then reading back goes through peekShowsThinking's override branch", () => {
  const { toggleShowThinking, peekShowsThinking } = makeThinkingPrecedence({
    hideThinkingDefault: true,
    peekTape: { "cell|squad-abc|0|1": ["some", "lines"] },
  });
  toggleShowThinking("cell|squad-abc|0|1", true);
  assert.equal(peekShowsThinking("cell|squad-abc|0|1"), true);
});
