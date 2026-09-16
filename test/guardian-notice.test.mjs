// Coverage for the board's popup (toast) that explains a guardian notice --
// the `set_guardian_notice`-backed, one-shot informational kinds still
// delivered this way (e.g. `forge_drift_interrupted_local`, RAL-299). As of
// RAL-451, `pr_merged_mid_flight` (a linked PR merging out-of-band while a
// rebase/feedback pass owned the review) no longer goes through this toast --
// it's routed through the dismissible mailbox widget instead (see
// `daemon/src/pr.rs`'s `settle_pr_merge_states` and `test/mailbox.test.mjs`).
//
// Run with `npm test` (node --test). See ./board-guardian-notice.mjs for how
// the view logic is loaded out of the real board.html.

import test from "node:test";
import assert from "node:assert/strict";
import { guardianNotice, boardSource } from "./board-guardian-notice.mjs";

const { pendingGuardianNoticeToasts } = guardianNotice;

/** A minimal GuardianView-shaped fixture for a forge-drift-interrupted-local notice. */
const FORGE_DRIFT_NOTICE = {
  id: "guardian-1",
  name: "demo",
  notice_kind: "forge_drift_interrupted_local",
  notice_message:
    "A GitHub/GitLab stack edit arrived while a local review edit was in progress; the newest base edit won.",
  notice_at_ms: 1000,
};

test("a guardian notice produces a popup explaining what happened", () => {
  const toasts = pendingGuardianNoticeToasts([FORGE_DRIFT_NOTICE], new Map());
  assert.equal(toasts.length, 1);
  const [toast] = toasts;
  assert.equal(toast.id, "guardian-1");
  assert.match(toast.text, /demo:/);
  assert.match(toast.text, /stack edit arrived/i);
});

test("a guardian with no notice produces no popup", () => {
  const toasts = pendingGuardianNoticeToasts(
    [{ id: "guardian-1", name: "demo", notice_kind: null, notice_message: null, notice_at_ms: null }],
    new Map(),
  );
  assert.deepEqual(toasts, []);
});

test("a notice already shown at the same notice_at_ms is not re-shown", () => {
  const shown = new Map([["guardian-1", FORGE_DRIFT_NOTICE.notice_at_ms]]);
  const toasts = pendingGuardianNoticeToasts([FORGE_DRIFT_NOTICE], shown);
  assert.deepEqual(toasts, []);
});

test("a fresh notice newer than what was shown is popped up again", () => {
  const shown = new Map([["guardian-1", 500]]);
  const toasts = pendingGuardianNoticeToasts([FORGE_DRIFT_NOTICE], shown);
  assert.equal(toasts.length, 1);
  assert.equal(toasts[0].notice_at_ms, 1000);
});

test("pendingGuardianNoticeToasts does not mutate the shown map itself", () => {
  const shown = new Map();
  pendingGuardianNoticeToasts([FORGE_DRIFT_NOTICE], shown);
  assert.equal(shown.size, 0, "the caller, not this pure function, records what was shown");
});

test("a notice with no message falls back to the notice kind", () => {
  const toasts = pendingGuardianNoticeToasts(
    [{ id: "guardian-1", name: "demo", notice_kind: "auto_build_failed", notice_message: null, notice_at_ms: 1 }],
    new Map(),
  );
  assert.match(toasts[0].text, /auto_build_failed/);
});

test("multiple guardians each newer than last-shown all produce a popup", () => {
  const list = [
    { ...FORGE_DRIFT_NOTICE, id: "guardian-1" },
    { ...FORGE_DRIFT_NOTICE, id: "guardian-2", name: "other" },
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
