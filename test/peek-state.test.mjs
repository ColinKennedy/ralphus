// RAL-186: automated coverage for board.html's live-view (peek) state machine.
//
// The bug this file exists for: opening "Show Live View" on a proof step,
// restarting that proof step, and coming back found the box still showing the
// stale "Historical record (read-only) — session ended" log. Only navigating
// to another node and back fixed it.
//
// The client-side half of that was an asymmetry — a `live -> ended` flip
// reported that the head banner needed re-rendering, an `ended -> live` flip
// did not (the comment justifying it assumed an unconditional 2s full
// re-render, which RAL-167 removed in favour of SSE push plus a 60s
// reconciliation fallback). The `headerChanged` assertions below are what pin
// that down: both directions must report it, or the banner/dot/tooltip silently
// stay wrong for as long as nothing else happens to re-render the pane.
//
// Run with `npm test` (node --test). See ./board-peek-state.mjs for how the
// state machine is loaded out of the real board.html.

import test from "node:test";
import assert from "node:assert/strict";
import { peek } from "./board-peek-state.mjs";

const { PEEK_MISSING_STRIKE_LIMIT, peekCssKey, peekUrlFor, nextPeekPaneState } = peek;

/** A fresh, never-fetched box's state — what fetchPeek derives from empty maps. */
const FRESH = { ended: false, missingStrikes: 0, lastActivityMs: null, text: "" };

/**
 * Folds a sequence of /pane responses through the state machine, returning the
 * transition produced by each one — i.e. exactly what repeated `pollOpenPeeks`
 * ticks do to one box.
 * @param {object} start initial state
 * @param {object[]} responses successive /pane payloads
 * @returns {object[]} one transition per response
 */
function poll(start, responses) {
  let state = start;
  return responses.map((data) => {
    const next = nextPeekPaneState(state, data, PEEK_MISSING_STRIKE_LIMIT);
    state = next.state;
    return next;
  });
}

const live = (content = "line one\n", at = 1_700_000_000_000) => ({
  active: true,
  content,
  last_activity_ms: at,
});
const gone = (content = "old historical output\n") => ({ active: false, content });

test("a live pane's content is shown as-is, with no ended banner", () => {
  const { state, headerChanged } = nextPeekPaneState(FRESH, live("hello\n"), PEEK_MISSING_STRIKE_LIMIT);
  assert.equal(state.ended, false);
  assert.equal(state.missingStrikes, 0);
  assert.equal(state.text, "hello\n");
  assert.equal(state.lastActivityMs, 1_700_000_000_000);
  assert.equal(headerChanged, false, "no flip happened, so no re-render should be forced");
});

test("an empty live pane reads as waiting, not as ended", () => {
  const { state } = nextPeekPaneState(FRESH, { active: true, content: "" }, PEEK_MISSING_STRIKE_LIMIT);
  assert.equal(state.ended, false);
  assert.equal(state.text, "(no output yet)");
  assert.equal(state.lastActivityMs, null);
});

test("a single inactive poll is absorbed as a transient miss, keeping the last content", () => {
  const started = nextPeekPaneState(FRESH, live("working…\n"), PEEK_MISSING_STRIKE_LIMIT).state;
  const { state, headerChanged } = nextPeekPaneState(started, gone(), PEEK_MISSING_STRIKE_LIMIT);
  assert.equal(state.ended, false, "one miss must not flash the box to 'ended'");
  assert.equal(state.missingStrikes, 1);
  assert.equal(state.text, "working…\n", "the last known live content stays on screen");
  assert.equal(headerChanged, false);
});

test("reaching the strike limit confirms the session ended and asks for a re-render", () => {
  const started = nextPeekPaneState(FRESH, live("working…\n"), PEEK_MISSING_STRIKE_LIMIT).state;
  const ticks = poll(started, Array(PEEK_MISSING_STRIKE_LIMIT).fill(gone("final output\n")));
  const last = ticks[ticks.length - 1];
  assert.equal(last.state.ended, true);
  assert.equal(last.state.lastActivityMs, null, "a finished cell's last output is history, not liveness");
  assert.match(last.state.text, /^final output\n/);
  assert.match(last.state.text, /\[Read-only historical record — this terminal session has ended\.\]$/);
  assert.equal(last.headerChanged, true, "live -> ended must force the banner/dot to re-render");
});

test("an ended session that never produced output says so explicitly", () => {
  const ticks = poll(FRESH, Array(PEEK_MISSING_STRIKE_LIMIT).fill({ active: false, content: "   \n" }));
  const last = ticks[ticks.length - 1];
  assert.equal(last.state.ended, true);
  assert.equal(last.state.text, "Terminal session has ended. No output was recorded before it ended.");
});

test("staying ended does not keep asking for re-renders", () => {
  const ended = poll(FRESH, Array(PEEK_MISSING_STRIKE_LIMIT).fill(gone())).pop().state;
  const more = poll(ended, [gone(), gone(), gone()]);
  assert.ok(
    more.every((t) => t.state.ended && t.headerChanged === false),
    "only the flip itself is a header change, not every subsequent ended poll",
  );
});

// ---- The RAL-186 regression proper ----

test("RAL-186: a restarted step's live view revives on the first active poll", () => {
  // Watch a step run, watch it finish (the restart tears the old pane down),
  // sit on the historical record for a couple of ticks while the squad is
  // re-queued, then the new tmux pane comes up under the same key.
  const ended = poll(FRESH, [live("attempt one\n"), gone("attempt one\n"), gone("attempt one\n"), gone("attempt one\n")]).pop().state;
  assert.equal(ended.ended, true, "precondition: the box is showing the historical record");

  const revived = nextPeekPaneState(ended, live("attempt two\n", 1_700_000_500_000), PEEK_MISSING_STRIKE_LIMIT);
  assert.equal(revived.state.ended, false, "the box must leave the read-only historical state");
  assert.equal(revived.state.missingStrikes, 0, "strikes must reset, or the next miss instantly re-ends it");
  assert.equal(revived.state.text, "attempt two\n", "the new pane's output, not the stale log");
  assert.equal(revived.state.lastActivityMs, 1_700_000_500_000, "liveness tracking resumes");
  assert.equal(
    revived.headerChanged,
    true,
    "ended -> live must force a re-render too: peekBox() is the only thing that " +
      "produces the 'Historical record (read-only)' banner, dot and tooltip, so " +
      "without this the box stays visually stuck until the user navigates away and back",
  );
});

test("RAL-186: recovery takes one poll tick, not an indefinite wait", () => {
  const ended = poll(FRESH, Array(PEEK_MISSING_STRIKE_LIMIT).fill(gone())).pop().state;
  const [first] = poll(ended, [live("fresh\n")]);
  assert.equal(first.state.ended, false);
  assert.equal(first.headerChanged, true);
});

test("RAL-186: a full restart cycle can repeat without the box latching", () => {
  // Two restarts back to back — the ended flag must not get stuck on either
  // pass, and each revival must announce itself.
  let state = FRESH;
  for (const attempt of ["one", "two", "three"]) {
    const up = nextPeekPaneState(state, live(`attempt ${attempt}\n`), PEEK_MISSING_STRIKE_LIMIT);
    assert.equal(up.state.ended, false, `attempt ${attempt} should be live`);
    state = up.state;
    const downTicks = poll(state, Array(PEEK_MISSING_STRIKE_LIMIT).fill(gone(`attempt ${attempt}\n`)));
    state = downTicks[downTicks.length - 1].state;
    assert.equal(state.ended, true, `attempt ${attempt} should end`);
  }
  const final = nextPeekPaneState(state, live("attempt four\n"), PEEK_MISSING_STRIKE_LIMIT);
  assert.equal(final.state.ended, false);
  assert.equal(final.headerChanged, true);
});

// ---- Key/URL derivation: the restarted *cell* case shares this machinery ----

test("cell and proof peek keys resolve to their own pane endpoints", () => {
  // Acceptance criterion: a restarted cell's live view goes through exactly
  // the same state machine as a restarted proof step's. The keys differ only in how
  // they address the daemon; both are index-derived and so survive a restart
  // unchanged, which is why the same fix covers both.
  assert.equal(
    peekUrlFor("cell|squad-abc|0|1"),
    "/api/squads/squad-abc/cells/0/1/pane?lines=500",
  );
  assert.equal(
    peekUrlFor("proof|squad-abc|0|task|-1|2"),
    "/api/squads/squad-abc/proofs/0/task/-1/2/pane?lines=500",
  );
  assert.equal(
    peekUrlFor("proof|squad-abc|0|cell|1|2"),
    "/api/squads/squad-abc/proofs/0/cell/1/2/pane?lines=500",
  );
  assert.equal(
    peekUrlFor("guardian|g1|b2"),
    "/api/guardians/g1/branches/b2/pane?lines=500",
  );
  assert.equal(
    peekUrlFor("guardian-manual|g1"),
    "/api/guardians/g1/manual-checks/pane?lines=500",
  );
  assert.equal(peekUrlFor("nonsense|whatever"), null);
});

test("every peek kind revives identically — the state machine is key-agnostic", () => {
  const keys = [
    "cell|squad-abc|0|1",
    "proof|squad-abc|0|task|-1|2",
    "proof|squad-abc|0|cell|1|2",
    "guardian|g1|b2",
    "guardian-manual|g1",
  ];
  for (const key of keys) {
    assert.ok(peekUrlFor(key), `${key} should address a pane endpoint`);
    const ended = poll(FRESH, Array(PEEK_MISSING_STRIKE_LIMIT).fill(gone())).pop().state;
    const revived = nextPeekPaneState(ended, live("back up\n"), PEEK_MISSING_STRIKE_LIMIT);
    assert.equal(revived.state.ended, false, `${key} should revive`);
    assert.equal(revived.headerChanged, true, `${key} should re-render its header on revival`);
  }
});

test("peek keys sanitize into stable, collision-free DOM ids", () => {
  assert.equal(peekCssKey("proof|squad-abc|0|task|-1|2"), "proof_squad-abc_0_task_-1_2");
  assert.equal(peekCssKey("cell|squad-abc|0|1"), "cell_squad-abc_0_1");
  // Restarting does not change any index, so the id a box is patched through
  // is the same before and after — the box the user is already looking at is
  // the one that gets revived.
  assert.equal(peekCssKey("proof|r|0|task|-1|0"), peekCssKey("proof|r|0|task|-1|0"));
  assert.notEqual(peekCssKey("proof|r|0|task|-1|0"), peekCssKey("proof|r|0|task|-1|1"));
});

test("the strike limit is a real tolerance, not a no-op", () => {
  assert.ok(
    PEEK_MISSING_STRIKE_LIMIT >= 2,
    "a limit of 1 would flash 'session ended' on every transient psmux hiccup",
  );
});
