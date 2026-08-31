// Coverage for the board's popup (toast) that explains a guardian notice,
// including the RAL-300 case: a linked PR merged out-of-band while a
// rebase/feedback pass owned the review, so the daemon drops the stale PR
// row and records a `pr_merged_mid_flight` notice for the board to surface.
//
// Run with `npm test` (node --test). See ./board-guardian-notice.mjs for how
// the view logic is loaded out of the real board.html.

import test from "node:test";
import assert from "node:assert/strict";
import { guardianNotice, boardSource } from "./board-guardian-notice.mjs";

const { pendingGuardianNoticeToasts } = guardianNotice;

/** A minimal GuardianView-shaped fixture for a dropped, out-of-band-merged PR notice. */
const DROPPED_PR_NOTICE = {
  id: "guardian-1",
  name: "demo",
  notice_kind: "pr_merged_mid_flight",
  notice_message:
    "A linked pull request merged on the forge while this review had a merge/feedback pass " +
    "in flight. It has been dropped from the review -- check whether any in-flight work still " +
    "applies, and resubmit a fresh PR if needed.",
  notice_at_ms: 1000,
};

test("a dropped, out-of-band-merged PR produces a popup explaining what happened", () => {
  const toasts = pendingGuardianNoticeToasts([DROPPED_PR_NOTICE], new Map());
  assert.equal(toasts.length, 1);
  const [toast] = toasts;
  assert.equal(toast.id, "guardian-1");
  assert.match(toast.text, /demo:/);
  assert.match(toast.text, /merged on the forge/i);
  assert.match(toast.text, /dropped from the review/i);
  assert.match(toast.text, /resubmit a fresh PR/i);
});

test("a guardian with no notice produces no popup", () => {
  const toasts = pendingGuardianNoticeToasts(
    [{ id: "guardian-1", name: "demo", notice_kind: null, notice_message: null, notice_at_ms: null }],
    new Map(),
  );
  assert.deepEqual(toasts, []);
});

test("a notice already shown at the same notice_at_ms is not re-shown", () => {
  const shown = new Map([["guardian-1", DROPPED_PR_NOTICE.notice_at_ms]]);
  const toasts = pendingGuardianNoticeToasts([DROPPED_PR_NOTICE], shown);
  assert.deepEqual(toasts, []);
});

test("a fresh notice newer than what was shown is popped up again", () => {
  const shown = new Map([["guardian-1", 500]]);
  const toasts = pendingGuardianNoticeToasts([DROPPED_PR_NOTICE], shown);
  assert.equal(toasts.length, 1);
  assert.equal(toasts[0].notice_at_ms, 1000);
});

test("pendingGuardianNoticeToasts does not mutate the shown map itself", () => {
  const shown = new Map();
  pendingGuardianNoticeToasts([DROPPED_PR_NOTICE], shown);
  assert.equal(shown.size, 0, "the caller, not this pure function, records what was shown");
});

test("a notice with no message falls back to the notice kind", () => {
  const toasts = pendingGuardianNoticeToasts(
    [{ id: "guardian-1", name: "demo", notice_kind: "pr_merged_mid_flight", notice_message: null, notice_at_ms: 1 }],
    new Map(),
  );
  assert.match(toasts[0].text, /pr_merged_mid_flight/);
});

test("multiple guardians each newer than last-shown all produce a popup", () => {
  const list = [
    { ...DROPPED_PR_NOTICE, id: "guardian-1" },
    { ...DROPPED_PR_NOTICE, id: "guardian-2", name: "other" },
  ];
  const toasts = pendingGuardianNoticeToasts(list, new Map());
  assert.equal(toasts.length, 2);
  assert.deepEqual(
    toasts.map((t) => t.id).sort(),
    ["guardian-1", "guardian-2"],
  );
});

// The assertion below is about wiring rather than pure logic: it reads the
// shipped board.html directly, because the code it covers (`showInfoToast`,
// the `_guardianNoticeShown` map update) needs a live document and so cannot
// be evaluated here.

test("checkGuardianNotices records the shown notice and toasts it", () => {
  const body = boardSource.slice(boardSource.indexOf("function checkGuardianNotices(list)"));
  const fn = body.slice(0, body.indexOf("\n      }\n") + 1);
  assert.match(fn, /pendingGuardianNoticeToasts\(list, _guardianNoticeShown\)/);
  const setAt = fn.indexOf("_guardianNoticeShown.set(");
  const toastAt = fn.indexOf("showInfoToast(toast.text)");
  assert.ok(setAt > -1 && toastAt > -1, "checkGuardianNotices shape changed");
});
