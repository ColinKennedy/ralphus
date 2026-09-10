// RAL-186 / RAL-397 Phase 2G-A: automated coverage for the board's live-view
// (peek) state machine.
//
// The bug this file was originally filed for (RAL-186): opening "Show Live
// View" on a proof step, restarting it, and coming back found the box still
// showing the stale "Historical record (read-only) — cell ended" log. Only
// navigating away and back fixed it. The client-side half was an asymmetry — a
// `live -> ended` flip reported that the head banner needed re-rendering, an
// `ended -> live` flip did not. The `headerChanged` assertions below pin that
// down: both directions must report it.
//
// RAL-397 Phase 2G-A changed the machine's *input*: the Live View no longer
// renders `/pane` content, so `nextPeekPaneState` no longer carries display
// text. It now folds a PeekLiveSignal — `live` (the `/pane` probe's `active`
// flag, or null if that probe failed) plus `grew` (did the transcript tape
// grow this poll) — into ended/liveness only. The content itself is rendered
// separately from the tape window.
//
// Run with `npm test` (node --test). See ./board-peek-state.mjs for how the
// state machine is loaded out of the real board chunks.

import test from "node:test";
import assert from "node:assert/strict";
import { peek } from "./board-peek-state.mjs";

const { PEEK_MISSING_STRIKE_LIMIT, peekCssKey, peekUrlFor, peekTranscriptUrlFor, nextPeekPaneState } = peek;

const NOW = 1_700_000_000_000;

/** A fresh, never-fetched box's state — what fetchPeek derives from empty maps. */
const FRESH = { ended: false, missingStrikes: 0, lastActivityMs: null };

/**
 * Folds a sequence of liveness signals through the state machine, returning the
 * transition produced by each — i.e. exactly what repeated `pollOpenPeeks`
 * ticks do to one box.
 * @param {object} start initial state
 * @param {object[]} signals successive PeekLiveSignal inputs
 * @returns {object[]} one transition per signal
 */
function poll(start, signals) {
  let state = start;
  return signals.map((signal) => {
    const next = nextPeekPaneState(state, signal, PEEK_MISSING_STRIKE_LIMIT);
    state = next.state;
    return next;
  });
}

/** A live signal: the `/pane` probe said active; `grew` says whether the tape advanced. */
const live = (grew = true, nowMs = NOW) => ({ live: true, grew, nowMs });
/** An authoritatively-ended signal: the `/pane` probe said inactive. */
const gone = () => ({ live: false, grew: false, nowMs: NOW });
/** A probe-failed signal: liveness unknown, so `grew` alone decides. */
const unknown = (grew, nowMs = NOW) => ({ live: null, grew, nowMs });

test("an active pane keeps the box live, with no ended banner", () => {
  const { state, headerChanged } = nextPeekPaneState(FRESH, live(true), PEEK_MISSING_STRIKE_LIMIT);
  assert.equal(state.ended, false);
  assert.equal(state.missingStrikes, 0);
  assert.equal(state.lastActivityMs, NOW, "a tape that grew stamps the activity time");
  assert.equal(headerChanged, false, "no flip happened, so no re-render should be forced");
});

test("an active but quiet pane stays live and keeps its prior activity time", () => {
  const started = nextPeekPaneState(FRESH, live(true, NOW), PEEK_MISSING_STRIKE_LIMIT).state;
  const { state } = nextPeekPaneState(started, live(false, NOW + 5000), PEEK_MISSING_STRIKE_LIMIT);
  assert.equal(state.ended, false);
  assert.equal(state.lastActivityMs, NOW, "no growth this poll => activity time is not bumped");
});

test("an authoritatively-inactive pane ends immediately — the probe is trusted", () => {
  const started = nextPeekPaneState(FRESH, live(true), PEEK_MISSING_STRIKE_LIMIT).state;
  const { state, headerChanged } = nextPeekPaneState(started, gone(), PEEK_MISSING_STRIKE_LIMIT);
  assert.equal(state.ended, true, "a definite `active:false` is not a transient miss");
  assert.equal(state.lastActivityMs, null, "a finished cell's last output is history, not liveness");
  assert.equal(headerChanged, true, "live -> ended must force the banner/dot to re-render");
});

test("staying ended does not keep asking for re-renders", () => {
  const ended = nextPeekPaneState(FRESH, gone(), PEEK_MISSING_STRIKE_LIMIT).state;
  const more = poll(ended, [gone(), gone(), gone()]);
  assert.ok(
    more.every((t) => t.state.ended && t.headerChanged === false),
    "only the flip itself is a header change, not every subsequent ended poll",
  );
});

// ---- Probe-failed fallback: liveness unknown, tape growth decides ----

test("when the liveness probe fails, a growing tape reads as live", () => {
  const { state, headerChanged } = nextPeekPaneState(FRESH, unknown(true), PEEK_MISSING_STRIKE_LIMIT);
  assert.equal(state.ended, false);
  assert.equal(state.missingStrikes, 0);
  assert.equal(headerChanged, false);
});

test("when the probe fails and the tape is quiet, a few polls are tolerated before ending", () => {
  const started = nextPeekPaneState(FRESH, live(true), PEEK_MISSING_STRIKE_LIMIT).state;
  const ticks = poll(started, Array(PEEK_MISSING_STRIKE_LIMIT).fill(unknown(false)));
  assert.equal(ticks[0].state.ended, false, "one unknown+quiet poll must not flash 'ended'");
  assert.equal(ticks[0].state.missingStrikes, 1);
  const last = ticks[ticks.length - 1];
  assert.equal(last.state.ended, true, "reaching the strike limit confirms ended");
  assert.equal(last.headerChanged, true, "the confirming flip forces a re-render");
});

// ---- The RAL-186 regression proper: revival must announce itself ----

test("RAL-186: a restarted step's live view revives on the first active poll", () => {
  const ended = nextPeekPaneState(FRESH, gone(), PEEK_MISSING_STRIKE_LIMIT).state;
  assert.equal(ended.ended, true, "precondition: the box is showing the historical record");

  const revived = nextPeekPaneState(ended, live(true, NOW + 500_000), PEEK_MISSING_STRIKE_LIMIT);
  assert.equal(revived.state.ended, false, "the box must leave the read-only historical state");
  assert.equal(revived.state.missingStrikes, 0, "strikes must reset, or the next miss instantly re-ends it");
  assert.equal(revived.state.lastActivityMs, NOW + 500_000, "liveness tracking resumes");
  assert.equal(
    revived.headerChanged,
    true,
    "ended -> live must force a re-render too: peekBox() is the only thing that " +
      "produces the 'Historical record (read-only)' banner, dot and tooltip, so " +
      "without this the box stays visually stuck until the user navigates away and back",
  );
});

test("RAL-186: recovery takes one poll tick, not an indefinite wait", () => {
  const ended = nextPeekPaneState(FRESH, gone(), PEEK_MISSING_STRIKE_LIMIT).state;
  const [first] = poll(ended, [live(true)]);
  assert.equal(first.state.ended, false);
  assert.equal(first.headerChanged, true);
});

test("RAL-186: a full restart cycle can repeat without the box latching", () => {
  let state = FRESH;
  for (const attempt of ["one", "two", "three"]) {
    const up = nextPeekPaneState(state, live(true), PEEK_MISSING_STRIKE_LIMIT);
    assert.equal(up.state.ended, false, `attempt ${attempt} should be live`);
    state = up.state;
    const down = nextPeekPaneState(state, gone(), PEEK_MISSING_STRIKE_LIMIT);
    state = down.state;
    assert.equal(state.ended, true, `attempt ${attempt} should end`);
  }
  const final = nextPeekPaneState(state, live(true), PEEK_MISSING_STRIKE_LIMIT);
  assert.equal(final.state.ended, false);
  assert.equal(final.headerChanged, true);
});

// ---- Key/URL derivation ----

test("peek keys resolve to their own /pane liveness-probe endpoints", () => {
  assert.equal(peekUrlFor("cell|squad-abc|0|1"), "/api/squads/squad-abc/cells/0/1/pane?lines=500");
  assert.equal(peekUrlFor("proof|squad-abc|0|task|-1|2"), "/api/squads/squad-abc/proofs/0/task/-1/2/pane?lines=500");
  assert.equal(peekUrlFor("guardian|g1|b2"), "/api/guardians/g1/branches/b2/pane?lines=500");
  assert.equal(peekUrlFor("guardian-manual|g1"), "/api/guardians/g1/manual-checks/pane?lines=500");
  assert.equal(peekUrlFor("nonsense|whatever"), null);
});

test("peek keys resolve to their own pane-transcript (tape) endpoints", () => {
  assert.equal(peekTranscriptUrlFor("cell|squad-abc|0|1"), "/api/squads/squad-abc/cells/0/1/pane-transcript");
  assert.equal(peekTranscriptUrlFor("proof|squad-abc|0|task|-1|2"), "/api/squads/squad-abc/proofs/0/task/-1/2/pane-transcript");
  assert.equal(peekTranscriptUrlFor("guardian|g1|b2"), "/api/guardians/g1/branches/b2/pane-transcript");
  assert.equal(peekTranscriptUrlFor("guardian-manual|g1"), "/api/guardians/g1/manual-checks/pane-transcript");
  assert.equal(peekTranscriptUrlFor("nonsense|whatever"), null);
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
    assert.ok(peekTranscriptUrlFor(key), `${key} should address a tape endpoint`);
    const ended = nextPeekPaneState(FRESH, gone(), PEEK_MISSING_STRIKE_LIMIT).state;
    const revived = nextPeekPaneState(ended, live(true), PEEK_MISSING_STRIKE_LIMIT);
    assert.equal(revived.state.ended, false, `${key} should revive`);
    assert.equal(revived.headerChanged, true, `${key} should re-render its header on revival`);
  }
});

test("peek keys sanitize into stable, collision-free DOM ids", () => {
  assert.equal(peekCssKey("proof|squad-abc|0|task|-1|2"), "proof_squad-abc_0_task_-1_2");
  assert.equal(peekCssKey("cell|squad-abc|0|1"), "cell_squad-abc_0_1");
  assert.equal(peekCssKey("proof|r|0|task|-1|0"), peekCssKey("proof|r|0|task|-1|0"));
  assert.notEqual(peekCssKey("proof|r|0|task|-1|0"), peekCssKey("proof|r|0|task|-1|1"));
});

test("the strike limit is a real tolerance, not a no-op", () => {
  assert.ok(
    PEEK_MISSING_STRIKE_LIMIT >= 2,
    "a limit of 1 would flash 'session ended' on every transient tape-quiet poll when the probe is down",
  );
});
