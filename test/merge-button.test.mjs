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
// start/resume/restart is happening; the toast wording acknowledges the
// *request* rather than claiming the rebase is done; and since RAL-423 the
// pending state clears on the daemon's response (the kickoff transition
// lands synchronously before it answers), with that response's own status
// patched into the local guardian copy -- the follow-up reload is fired and
// forgotten rather than awaited.
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
  "finalizing",
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

test("a manual rebase advertises that it renews automatic CI-fix eligibility", () => {
  assert.match(mergeButtonView("in_review", false).tip, /fresh automatic CI-fix attempt/i);
  assert.match(mergeButtonView("merge_stopped", false).tip, /fresh automatic CI-fix attempt/i);
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
  for (const status of ["merging", "finalizing", "approved", "cancelled", "deployed"]) {
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
  const clearAt = fn.indexOf("pendingMergeActions.delete(id)");
  const tickAt = fn.indexOf("tick();");
  assert.ok(
    pendingAt > -1 && toastAt > -1 && awaitAt > -1 && clearAt > -1 && tickAt > -1,
    "mergeReview shape changed",
  );
  assert.ok(pendingAt < awaitAt, "the pending state must not wait on the daemon");
  assert.ok(toastAt < awaitAt, "the toast must not wait on the daemon");
});

test("mergeReview clears pending on the daemon's response, not after the reload", () => {
  const body = boardSource.slice(boardSource.indexOf("async function mergeReview(id, status)"));
  const fn = body.slice(0, body.indexOf("\n      }\n") + 1);
  const awaitAt = fn.indexOf("await guardianAction(");
  const clearAt = fn.indexOf("pendingMergeActions.delete(id)");
  const tickAt = fn.indexOf("tick();");
  assert.ok(awaitAt > -1 && clearAt > -1 && tickAt > -1, "mergeReview shape changed");
  assert.equal(fn.indexOf("await tick()"), -1, "the reload must be fired and forgotten, not awaited");
  assert.ok(clearAt > awaitAt, "the pending state must clear right after the daemon answers");
  // The daemon's claim precedes its response (`kickoff_merge` transitions
  // the guardian synchronously), so the response body's own status drives
  // what the button shows until the reload lands -- without the patch the
  // button would flip back to startable in the gap between the response
  // and the reload.
  assert.ok(fn.indexOf('body.status === "merging"') > -1, "the local status must reflect a claimed merge");
  assert.ok(fn.indexOf('body.status === "approved"') > -1, "the local status must reflect an already-merged review");
});

// The daemon claims the review before it answers, so once `guardianAction`
// resolves the rebase has already started. `tick()` reloads the whole board
// and ends in `pollReviews`, which waits on every open PR's drift check --
// awaiting it here is what left the button reading "Starting…" for tens of
// seconds after the rebase was underway. The button stays correctly disabled
// without the pending flag, because `merging` is not in MERGE_STARTABLE.
test("mergeReview's pending state ends with the daemon's answer, not the board reload", () => {
  const body = boardSource.slice(boardSource.indexOf("async function mergeReview(id, status)"));
  const fn = body.slice(0, body.indexOf("\n      }\n") + 1);
  assert.doesNotMatch(fn, /await tick\(\)/, "the pending state must not span a full board reload");
  const clearAt = fn.indexOf("pendingMergeActions.delete(id)");
  const tickAt = fn.indexOf("tick();");
  assert.ok(clearAt < tickAt, "the pending state must be cleared before the reload is kicked off");
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
