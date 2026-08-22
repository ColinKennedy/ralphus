// RAL-232: automated coverage for board.html's Live View debug-line
// stripping — the "Show Debug Messages" checkbox's underlying logic.
//
// Ralphus interleaves its own diagnostic/telemetry lines (`ralphus [TYPE]
// ...`, `RALPHUS_EVENT: ...`, `RALPHUS_TMUX_DONE`) into the same tmux pane
// the agent's own output streams through (see .agent/logging-policy.md and
// runner/src/{execute,cartographer,main}.rs). Unchecked (the default), the
// Live View should show only what the agent produced.
//
// Run with `npm test` (node --test). See ./board-debug-strip.mjs for how
// this logic is loaded out of the real board.html.

import test from "node:test";
import assert from "node:assert/strict";
import { debugStrip } from "./board-debug-strip.mjs";

const { isRalphusDebugLine, stripDebugLines } = debugStrip;

// ---- isRalphusDebugLine ----

test("recognizes each documented ralphus log-line prefix", () => {
  assert.equal(isRalphusDebugLine("ralphus [runner] invoked squad=squad-1 cell=c1 agent=\"claude\""), true);
  assert.equal(isRalphusDebugLine("ralphus [llm] start squad=squad-1 cell=c1 agent=\"claude\""), true);
  assert.equal(isRalphusDebugLine("ralphus [llm-invoke] start agent=\"claude\" model=\"opus\""), true);
  assert.equal(isRalphusDebugLine('RALPHUS_EVENT: {"source":"claude-code","message":"session-id known"}'), true);
  assert.equal(isRalphusDebugLine("RALPHUS_TMUX_DONE: 0"), true);
  assert.equal(isRalphusDebugLine("RALPHUS_TMUX_DONE"), true);
});

test("tolerates leading whitespace before the ralphus prefix", () => {
  assert.equal(isRalphusDebugLine("   ralphus [proof] starting cwd=/repo command=\"cargo test\""), true);
  assert.equal(isRalphusDebugLine("\tRALPHUS_EVENT: {}"), true);
});

test("agent-produced lines are never classified as ralphus lines", () => {
  assert.equal(isRalphusDebugLine("[tool] Read(file.txt)"), false);
  assert.equal(isRalphusDebugLine("[result] file contents here"), false);
  assert.equal(isRalphusDebugLine("[error] something went wrong"), false);
  assert.equal(isRalphusDebugLine("I'll go ahead and fix the bug now."), false);
  assert.equal(isRalphusDebugLine("RALPHUS_PROOF: PASS"), false, "the proof verdict marker is agent-produced, not ralphus telemetry");
});

test("a mid-line (not line-start) occurrence of a ralphus marker is not a false positive", () => {
  // Mirrors daemon/src/runner.rs's pane_shows_done_sentinel guard: an agent
  // grepping/reading its own source (which contains these strings verbatim)
  // must not have its own tool output misclassified as a ralphus line.
  assert.equal(isRalphusDebugLine("runner.rs:34:const EVENT_MARKER: &str = \"RALPHUS_EVENT: \";"), false);
  assert.equal(isRalphusDebugLine("34: if trimmed.starts_with(\"ralphus [\") { ... }"), false);
  assert.equal(isRalphusDebugLine("grep: found RALPHUS_TMUX_DONE in runner/src/main.rs"), false);
});

// ---- stripDebugLines ----

test("agent-only lines pass through unchanged", () => {
  const text = "[tool] Read(file.txt)\n[result] file contents here\nI'm done reading the file.";
  assert.equal(stripDebugLines(text), text);
});

test("ralphus-only lines are removed entirely, leaving an empty string", () => {
  const text = [
    "ralphus [runner] invoked squad=squad-1 cell=c1",
    'RALPHUS_EVENT: {"source":"claude-code"}',
    "RALPHUS_TMUX_DONE: 0",
  ].join("\n");
  assert.equal(stripDebugLines(text), "");
});

test("a ralphus line interleaved mid-tool-call is dropped without corrupting the agent lines around it", () => {
  const text = [
    "[tool] Read(file.txt)",
    "ralphus [llm] done squad=squad-1 cell=c1 tokens_in=10 tokens_out=20 cost_usd=0.01",
    "[result] file contents here",
  ].join("\n");
  assert.equal(stripDebugLines(text), "[tool] Read(file.txt)\n[result] file contents here");
});

test("multiple ralphus lines scattered through agent output are all removed, order preserved", () => {
  const text = [
    "ralphus [runner] invoked squad=squad-1 cell=c1 agent=\"claude\"",
    "Claude Code · model=default",
    "cwd: /repo",
    "",
    "I'll start by reading the file.",
    "[tool] Read(file.txt)",
    "ralphus [llm-invoke] start agent=\"claude\" model=\"opus\"",
    "[result] contents",
    "ralphus [llm-invoke] done agent=\"claude\" elapsed=1.20s tokens_in=5 tokens_out=9",
    "Done.",
    "RALPHUS_TMUX_DONE: 0",
  ].join("\n");
  const expected = [
    "Claude Code · model=default",
    "cwd: /repo",
    "",
    "I'll start by reading the file.",
    "[tool] Read(file.txt)",
    "[result] contents",
    "Done.",
  ].join("\n");
  assert.equal(stripDebugLines(text), expected);
});

test("a false-positive-guarded ralphus-looking string embedded in agent prose survives", () => {
  const text = "The runner emits a line like `ralphus [llm] done ...` for every LLM call.";
  assert.equal(stripDebugLines(text), text);
});

test("empty input stays empty", () => {
  assert.equal(stripDebugLines(""), "");
});

test("a lone blank line is preserved (agent output can legitimately contain blank lines)", () => {
  assert.equal(stripDebugLines("first\n\nsecond"), "first\n\nsecond");
});
