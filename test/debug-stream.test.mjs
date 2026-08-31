// RAL-296: automated coverage for board.html's unified debug/terminal-log
// stream — the client-side half of collapsing what used to be two
// differently-sourced views (a disconnected Cartographer-only footer on the
// Live View pane, vs. the durable-attempt viewer reading raw, un-merged
// terminal-log content) onto the daemon's one `.../debug-events` endpoint
// (`daemon/src/timeline.rs::entity_debug_timeline`).
//
// What is pinned here:
//  - `debugEventsUrlFor` addresses the right endpoint for all four terminal-
//    log contexts (cell, proof step, guardian branch resolver, guardian
//    manual-checks) now that RAL-296 wired up the latter three.
//  - `formatDebugEvent` inlines a terminal-log excerpt under its own event
//    rather than dropping it.
//  - `isMostRecentAttempt` — the "current-attempt-only" decision that routes
//    the attempt-history popup through the merged stream only for the most
//    recent attempt, leaving older attempts reading their own raw content.
//
// Run with `npm test` (node --test). See ./board-debug-stream.mjs for how
// this logic is loaded out of the real board.html.

import test from "node:test";
import assert from "node:assert/strict";
import { debugStream, boardSource } from "./board-debug-stream.mjs";

const { debugEventsUrlFor, formatDebugEvent, isMostRecentAttempt } = debugStream;

// ---- debugEventsUrlFor: all four scopes ----

test("a task cell's debug-events URL addresses the cell endpoint", () => {
  assert.equal(
    debugEventsUrlFor("cell|squad-abc|0|1"),
    "/api/squads/squad-abc/cells/0/1/debug-events",
  );
});

test("a proof step's debug-events URL addresses the proof endpoint, task-cell and squad-cell scoped alike", () => {
  assert.equal(
    debugEventsUrlFor("proof|squad-abc|0|task|-1|2"),
    "/api/squads/squad-abc/proofs/0/task/-1/2/debug-events",
  );
  assert.equal(
    debugEventsUrlFor("proof|squad-abc|0|cell|1|2"),
    "/api/squads/squad-abc/proofs/0/cell/1/2/debug-events",
  );
});

test("a guardian branch resolver's debug-events URL addresses the branch endpoint", () => {
  assert.equal(
    debugEventsUrlFor("guardian|g1|b2"),
    "/api/guardians/g1/branches/b2/debug-events",
  );
});

test("a guardian manual-checks run's debug-events URL addresses the manual-checks endpoint", () => {
  assert.equal(
    debugEventsUrlFor("guardian-manual|g1"),
    "/api/guardians/g1/manual-checks/debug-events",
  );
});

test("an unrecognized peek kind has no debug-events endpoint", () => {
  assert.equal(debugEventsUrlFor("nonsense|whatever"), null);
});

test("every peek kind's pane and debug-events URLs share the same entity path", () => {
  // Acceptance criterion for RAL-296: the endpoint this box's checkbox reads
  // from must address the exact same entity its pane already reads from —
  // otherwise "Show Debug Messages" could show a different cell's events.
  const cases = [
    ["cell|squad-abc|0|1", "/api/squads/squad-abc/cells/0/1"],
    ["proof|squad-abc|0|task|-1|2", "/api/squads/squad-abc/proofs/0/task/-1/2"],
    ["guardian|g1|b2", "/api/guardians/g1/branches/b2"],
    ["guardian-manual|g1", "/api/guardians/g1/manual-checks"],
  ];
  for (const [key, prefix] of cases) {
    assert.equal(debugEventsUrlFor(key), `${prefix}/debug-events`, key);
  }
});

// ---- formatDebugEvent ----

test("an event with no terminal-log excerpt renders as one line", () => {
  const line = formatDebugEvent({
    at_ms: 1_700_000_000_000,
    level: "info",
    source: "runner",
    message: "invoked",
  });
  assert.equal(line, `[${new Date(1_700_000_000_000).toLocaleTimeString()}] runner: invoked`);
  assert.doesNotMatch(line, /\n/);
});

test("an event with a terminal-log excerpt inlines it indented beneath the event line", () => {
  const line = formatDebugEvent({
    at_ms: 1_700_000_000_000,
    level: "info",
    source: "runner",
    message: "terminal log attempt 0 written",
    log_excerpt: "line one\nline two",
  });
  const lines = line.split("\n");
  assert.equal(lines.length, 3);
  assert.match(lines[0], /terminal log attempt 0 written$/);
  assert.equal(lines[1], "    | line one");
  assert.equal(lines[2], "    | line two");
});

test("an empty-string excerpt is treated the same as no excerpt", () => {
  const line = formatDebugEvent({ at_ms: 0, level: "info", source: "s", message: "m", log_excerpt: "" });
  assert.doesNotMatch(line, /\n/);
});

// ---- isMostRecentAttempt: the "current-attempt-only" routing decision ----

test("the highest-numbered attempt in the list is the most recent", () => {
  const attempts = [{ attempt: 0 }, { attempt: 1 }, { attempt: 2 }];
  assert.equal(isMostRecentAttempt(attempts, 2), true);
});

test("an older attempt in the list is not the most recent", () => {
  const attempts = [{ attempt: 0 }, { attempt: 1 }, { attempt: 2 }];
  assert.equal(isMostRecentAttempt(attempts, 0), false);
  assert.equal(isMostRecentAttempt(attempts, 1), false);
});

test("attempts do not need to be sorted for the decision to hold", () => {
  const attempts = [{ attempt: 2 }, { attempt: 0 }, { attempt: 1 }];
  assert.equal(isMostRecentAttempt(attempts, 2), true);
  assert.equal(isMostRecentAttempt(attempts, 1), false);
});

test("an unfetched (empty) attempt list never counts as the most recent", () => {
  // viewHistoryAttempt can be called before historyAttempts[key] is
  // populated; falling back to "most recent" here would silently reroute an
  // unrelated attempt through the merged stream instead of failing safe.
  assert.equal(isMostRecentAttempt([], 0), false);
});

test("a single-attempt history's only attempt is the most recent", () => {
  assert.equal(isMostRecentAttempt([{ attempt: 0 }], 0), true);
});

// ---- Wiring: viewHistoryAttempt routes only the most recent attempt through the merged stream ----
// These read the shipped board.html directly, because the routing decision
// combines module-level state (historyAttempts) with the pure helpers above
// and so cannot be evaluated in isolation the way the helpers themselves can.

test("viewHistoryAttempt picks the debug-events URL only for the most recent attempt", () => {
  const body = boardSource.slice(boardSource.indexOf("async function viewHistoryAttempt(key, attempt)"));
  const fn = body.slice(0, body.indexOf("\n      }\n") + 1);
  assert.match(fn, /isMostRecentAttempt\(attempts, attempt\)/);
  assert.match(fn, /debugUrl = isMostRecentAttempt\(attempts, attempt\) \? debugEventsUrlFor\(key\) : null/);
  const fallbackAt = fn.indexOf("terminalLogAttemptsUrlFor(key, attempt)");
  assert.ok(fallbackAt > -1, "an older attempt must still fall back to its own raw terminal-log content");
});

test("currentPeekDisplayText places debug events chronologically ahead of the live pane, not appended after it", () => {
  const body = boardSource.slice(boardSource.indexOf("function currentPeekDisplayText(key)"));
  const fn = body.slice(0, body.indexOf("\n      }\n") + 1);
  assert.match(fn, /`\$\{events\.map\(formatDebugEvent\)\.join\("\\n"\)\}\\n\\n\$\{base\}`/);
});
