// Coverage for the board's "Merge / rebase" click acknowledgement.
//
// The bug this file exists for: clicking "Merge / rebase" gave no visual
// feedback whatsoever. The daemon-side kickoff is a DB state transition plus
// a thread spawn, but it was queued behind whatever slow board poll the
// synchronous accept loop was already inside (see
// daemon/tests/http_concurrency.rs for that half), and even once that was
// fixed the button still did not change until the follow-up board reload
// landed. Silence for that long trains people to click it twice.
//
// What is pinned here: the button reports a pending, disabled state the
// instant it is clicked; the label and tooltip stay honest about which of
// start/resume/restart is happening; and the toast wording acknowledges the
// *request* rather than claiming the rebase is done.
//
// Run with `npm test` (node --test). See ./board-merge-button.mjs for how the
// view logic is loaded out of the real board.html.

import test from "node:test";
import assert from "node:assert/strict";
import { mergeButton, boardSource } from "./board-merge-button.mjs";

const { MERGE_STARTABLE, mergeButtonView, mergeRequestedToast } = mergeButton;

/** Every status the daemon's guardian state machine can report. */
const ALL_STATUSES = [
  "collecting",
  "merging",
  "in_review",
  "merge_failed",
  "merge_stopped",
  "approved",
  "cancelled",
  "deployed",
];

test("a fresh rebase reads as Merge / rebase and is clickable", () => {
  for (const status of ["collecting", "in_review", "merge_failed"]) {
    const view = mergeButtonView(status, false);
    assert.equal(view.label, "Merge / rebase", status);
    assert.equal(view.enabled, true, status);
  }
});

test("a stopped review offers Resume rebase, not a fresh merge", () => {
  const view = mergeButtonView("merge_stopped", false);
  assert.equal(view.label, "Resume rebase");
  assert.equal(view.enabled, true);
  assert.match(view.tip, /resume the stopped rebase/i);
});

test("a click is acknowledged immediately: pending disables the button", () => {
  for (const status of MERGE_STARTABLE) {
    const view = mergeButtonView(status, true);
    assert.equal(view.enabled, false, `${status} must not be clickable while a kickoff is in flight`);
    assert.notEqual(
      view.label,
      mergeButtonView(status, false).label,
      `${status} must visibly change the instant it is clicked`,
    );
  }
});

test("the pending label distinguishes resuming from starting", () => {
  assert.equal(mergeButtonView("merge_stopped", true).label, "Resuming…");
  assert.equal(mergeButtonView("collecting", true).label, "Starting…");
});

test("the pending tooltip says the request was sent, not that the rebase finished", () => {
  const tip = mergeButtonView("collecting", true).tip;
  assert.match(tip, /request has been sent/i);
  assert.match(tip, /background/i);
  assert.doesNotMatch(tip, /\b(finished|complete|done)\b/i);
});

test("every status produces a non-empty tooltip", () => {
  for (const status of ALL_STATUSES) {
    for (const pending of [false, true]) {
      const view = mergeButtonView(status, pending);
      assert.ok(view.tip.length > 0, `${status} pending=${pending} needs a data-tip`);
      assert.ok(view.label.length > 0, `${status} pending=${pending} needs a label`);
    }
  }
});

test("an unknown status falls back to a disabled button with a real explanation", () => {
  const view = mergeButtonView("something-new", false);
  assert.equal(view.enabled, false);
  assert.match(view.tip, /something-new/);
});

test("statuses the daemon refuses a merge from are disabled and say why", () => {
  for (const status of ["merging", "approved", "cancelled", "deployed"]) {
    const view = mergeButtonView(status, false);
    assert.equal(view.enabled, false, status);
    assert.ok(view.tip.length > 0, status);
  }
});

test("the toast acknowledges the request rather than the rebase finishing", () => {
  for (const status of ["collecting", "in_review", "merge_failed", "merge_stopped", "merging"]) {
    const message = mergeRequestedToast(status);
    assert.match(message, /requested/i, status);
    assert.match(message, /background/i, status);
    assert.doesNotMatch(message, /\b(rebased|finished|complete[d]?)\b/i, status);
  }
});

test("the toast wording tracks which of start/resume/restart was asked for", () => {
  assert.match(mergeRequestedToast("collecting"), /^Rebase requested/);
  assert.match(mergeRequestedToast("merge_stopped"), /^Resume requested/);
  assert.match(mergeRequestedToast("merging"), /^Restart requested/);
});

// The three assertions below are about wiring rather than pure logic: they
// read the shipped board.html directly, because the code they cover needs a
// live document and so cannot be evaluated here.

test("mergeReview marks the button pending and toasts before awaiting the daemon", () => {
  const body = boardSource.slice(boardSource.indexOf("async function mergeReview(id, status)"));
  const fn = body.slice(0, body.indexOf("\n      }\n") + 1);
  const pendingAt = fn.indexOf("pendingMergeActions.add(id)");
  const toastAt = fn.indexOf("showInfoToast(mergeRequestedToast(status))");
  const awaitAt = fn.indexOf("await guardianAction(");
  const tickAt = fn.indexOf("await tick()");
  assert.ok(pendingAt > -1 && toastAt > -1 && awaitAt > -1 && tickAt > -1, "mergeReview shape changed");
  assert.ok(pendingAt < awaitAt, "the pending state must not wait on the daemon");
  assert.ok(toastAt < awaitAt, "the toast must not wait on the daemon");
  assert.ok(toastAt < tickAt, "the toast must not wait on the board reload");
});

test("mergeReview refuses a second submission while one is in flight", () => {
  const body = boardSource.slice(boardSource.indexOf("async function mergeReview(id, status)"));
  const fn = body.slice(0, body.indexOf("\n      }\n") + 1);
  assert.match(fn, /if \(pendingMergeActions\.has\(id\)\) return;/);
});

test("mergeReview clears the pending state whatever the daemon answers", () => {
  const body = boardSource.slice(boardSource.indexOf("async function mergeReview(id, status)"));
  const fn = body.slice(0, body.indexOf("\n      }\n") + 1);
  const finallyAt = fn.indexOf("} finally {");
  const deleteAt = fn.indexOf("pendingMergeActions.delete(id)");
  assert.ok(finallyAt > -1 && deleteAt > finallyAt, "the clear must run in a finally block");
});
