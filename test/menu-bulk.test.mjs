// Coverage for RAL-508 "Apply squad actions to multiselections":
//
// - menuActionTargets resolves a context-menu action's target set: the whole
//   active multi-selection when the clicked item belongs to it, otherwise
//   just the clicked item -- the item under the cursor never reduces an
//   active selection to itself alone;
// - bulkEligibleSplit splits a mixed-eligibility selection into the
//   applicable ids and a skipped count that gets *reported*, never silently
//   dropped;
// - bulkActEach applies each id exactly once, and one item failing does not
//   block the rest of the batch;
// - reportBulkOutcome communicates skips and per-item failures, and only
//   claims success when every selected item was applied;
// - bulkNameList caps very long name lists for confirmation dialogs;
// - the shipped bulk squad-activate, bulk squad-retry, and bulk
//   review-cancel actions route through those primitives: one shared
//   confirmation, one request per eligible item, skips and failures
//   reported;
// - the squad and review context menus disable their popup-bearing items
//   (Rename / Watch / Logs, Edit Details / Watch) while a multi-selection is
//   active instead of silently applying them to one row;
// - right-clicking a selected squad routes Retry / Restart / Cancel / Delete
//   / Set Status into their bulk paths, while a right-click outside the
//   selection falls back to the clicked row alone;
// - Set Status's bulk picker applies the chosen state to every selected
//   squad exactly once, with per-item failure reporting;
// - destructive bulk confirmations (squad delete, review delete, bulk
//   restart, bulk cancel's cascade preview) list every affected item name
//   before applying, then report a per-item success/failure summary;
// - hide/unhide routing applies the whole selection only when the clicked
//   item is part of it.
//
// Run with `npm test` (node --test). See ./board-menu-bulk.mjs for how the
// regions are loaded out of the real board chunks.

import test from "node:test";
import assert from "node:assert/strict";
import { makeBulkHelpers, makeBulkAction, makeSquadMenu, makeReviewMenu, makeStatusRoute, makeSquadMenuRoute } from "./board-menu-bulk.mjs";

// ---------- menuActionTargets ----------

test("a click inside a multi-selection targets every selected id", () => {
  const h = makeBulkHelpers();
  const sel = new Set(["sq-1", "sq-2", "sq-3"]);
  assert.deepEqual(h.menuActionTargets("sq-2", sel), ["sq-1", "sq-2", "sq-3"]);
});

test("a click outside the selection targets just the clicked item", () => {
  const h = makeBulkHelpers();
  const sel = new Set(["sq-1", "sq-2", "sq-3"]);
  assert.deepEqual(h.menuActionTargets("sq-9", sel), ["sq-9"]);
});

test("a lone selected item resolves to just itself, not a widened set", () => {
  const h = makeBulkHelpers();
  assert.deepEqual(h.menuActionTargets("sq-1", new Set(["sq-1"])), ["sq-1"]);
});

// ---------- bulkEligibleSplit ----------

test("mixed-eligibility ids split into eligible ids and a skipped count", () => {
  const h = makeBulkHelpers();
  const { eligible, skipped } = h.bulkEligibleSplit(["a", "b", "c", "d"], (id) => id !== "b");
  assert.deepEqual(eligible, ["a", "c", "d"]);
  assert.equal(skipped, 1);
});

test("an all-eligible selection reports zero skipped", () => {
  const h = makeBulkHelpers();
  const { eligible, skipped } = h.bulkEligibleSplit(["a", "b"], () => true);
  assert.deepEqual(eligible, ["a", "b"]);
  assert.equal(skipped, 0);
});

// ---------- bulkActEach ----------

test("bulkActEach applies every id exactly once, in order", async () => {
  const h = makeBulkHelpers();
  const seen = [];
  const failed = await h.bulkActEach(["a", "b", "c"], (id) => id, async (id) => {
    seen.push(id);
    return { ok: true };
  }, "x failed");
  assert.deepEqual(seen, ["a", "b", "c"]);
  assert.deepEqual(failed, []);
});

test("one failing item does not block the rest of the batch", async () => {
  const h = makeBulkHelpers();
  const seen = [];
  const failed = await h.bulkActEach(["a", "b", "c"], (id) => `L:${id}`, async (id) => {
    seen.push(id);
    return id === "b" ? { ok: false } : { ok: true };
  }, "x failed");
  assert.deepEqual(seen, ["a", "b", "c"]);
  assert.deepEqual(failed, ["L:b: srv error"]);
});

test("a thrown request is reported as a network error, not a crash", async () => {
  const h = makeBulkHelpers();
  const failed = await h.bulkActEach(["a"], (id) => id, async () => {
    throw new Error("offline");
  }, "x failed");
  assert.deepEqual(failed, ["a: network error"]);
});

// ---------- reportBulkOutcome ----------

test("a fully applied batch reports one success and nothing else", () => {
  const h = makeBulkHelpers();
  h.reportBulkOutcome("Deleted", "squad", 3, 0, "", []);
  assert.deepEqual(h.calls.notifications, [{ kind: "success", msg: "Deleted 3 squad(s)." }]);
});

test("skipped ineligible items are reported as an error, never silently dropped", () => {
  const h = makeBulkHelpers();
  h.reportBulkOutcome("Activated", "squad", 2, 1, "not queued", []);
  assert.equal(h.calls.notifications.length, 1);
  assert.equal(h.calls.notifications[0].kind, "error");
  assert.match(h.calls.notifications[0].msg, /Skipped 1 of the selected squad/);
  assert.match(h.calls.notifications[0].msg, /not queued/);
});

test("per-item failures are reported and suppress the success summary", () => {
  const h = makeBulkHelpers();
  h.reportBulkOutcome("Cancelled", "review", 2, 0, "", ["r1: daemon said no"]);
  assert.deepEqual(h.calls.notifications, [{ kind: "error", msg: "r1: daemon said no" }]);
});

test("skips and failures are both reported; still no success summary", () => {
  const h = makeBulkHelpers();
  h.reportBulkOutcome("Retried", "squad", 1, 2, "not finished", ["s1: daemon said no"]);
  assert.equal(h.calls.notifications.length, 2);
  assert.deepEqual(h.calls.notifications.map((n) => n.kind), ["error", "error"]);
});

test("nothing applied and nothing to report stays silent", () => {
  const h = makeBulkHelpers();
  h.reportBulkOutcome("Retried", "squad", 0, 0, "", []);
  assert.deepEqual(h.calls.notifications, []);
});

// ---------- bulkNameList ----------

test("bulkNameList lists every name under the cap", () => {
  const h = makeBulkHelpers();
  assert.equal(h.bulkNameList(["a", "b", "c"]), "a\nb\nc");
});

test("bulkNameList caps long lists with a remaining count", () => {
  const h = makeBulkHelpers();
  const out = h.bulkNameList(Array.from({ length: 25 }, (_, i) => `s${i}`));
  const lines = out.split("\n");
  assert.equal(lines.length, 21);
  assert.equal(lines[20], "... and 5 more");
});

// ---------- bulkActivateSquads (shipped squad-menu bulk action) ----------

test("bulk activate posts once per queued squad and skips the rest with a report", async () => {
  const h = makeBulkAction({
    action: "squadActivate",
    squads: { q1: { label: "Queued One", state: "queued" }, q2: { label: "Queued Two", state: "queued" }, p1: { label: "Pending One", state: "pending" } },
  });
  await h.bulkActivateSquads(["q1", "p1", "q2"]);
  assert.deepEqual(h.calls.posts, ["/api/squads/q1/activate", "/api/squads/q2/activate"]);
  const kinds = h.calls.notifications.map((n) => n.kind);
  assert.ok(kinds.includes("error"), "the skipped pending squad must be reported");
  assert.ok(h.calls.notifications.some((n) => n.msg.includes("Skipped 1 of the selected squad") && n.msg.includes("not queued")));
  assert.equal(h.calls.ticks, 1);
});

test("bulk activate reports success only when every selected squad was queued", async () => {
  const h = makeBulkAction({
    action: "squadActivate",
    squads: { q1: { state: "queued" }, q2: { state: "queued" } },
  });
  await h.bulkActivateSquads(["q1", "q2"]);
  assert.deepEqual(h.calls.posts.length, 2);
  assert.deepEqual(h.calls.notifications, [{ kind: "success", msg: "Activated 2 squad(s)." }]);
});

test("bulk activate reports a per-item failure without blocking the others", async () => {
  const h = makeBulkAction({
    action: "squadActivate",
    squads: { q1: { label: "Q1", state: "queued" }, q2: { label: "Q2", state: "queued" } },
    failIds: ["q1"],
  });
  await h.bulkActivateSquads(["q1", "q2"]);
  assert.deepEqual(h.calls.posts, ["/api/squads/q1/activate", "/api/squads/q2/activate"]);
  assert.deepEqual(h.calls.notifications, [{ kind: "error", msg: "Q1: daemon said no" }]);
});

// ---------- bulkRetrySquads (shipped squad-menu bulk action) ----------

test("bulk retry retries only terminal squads, once each, and reports skips", async () => {
  const h = makeBulkAction({
    action: "squadRetry",
    squads: { d1: { label: "Done One", state: "done" }, r1: { label: "Running", state: "running" }, f1: { label: "Failed One", state: "failed" } },
    promptResult: "retry",
  });
  await h.bulkRetrySquads(["d1", "r1", "f1"]);
  assert.deepEqual(h.calls.posts, ["/api/squads/d1/retry", "/api/squads/f1/retry"]);
  assert.ok(h.calls.notifications.some((n) => n.msg.includes("Skipped 1 of the selected squad") && n.msg.includes("not finished")));
});

test("bulk retry keeps the type-to-confirm friction when a selected squad succeeded", async () => {
  const h = makeBulkAction({
    action: "squadRetry",
    squads: { d1: { label: "Done One", state: "done" }, f1: { label: "Failed One", state: "failed" } },
    promptResult: "retry",
  });
  await h.bulkRetrySquads(["d1", "f1"]);
  assert.equal(h.calls.promptTexts.length, 1);
  assert.match(h.calls.promptTexts[0], /SUCCEEDED/);
  assert.match(h.calls.promptTexts[0], /Done One/);
  assert.deepEqual(h.calls.posts, ["/api/squads/d1/retry", "/api/squads/f1/retry"]);
});

test("bulk retry aborts without posting when the type-to-confirm answer is wrong", async () => {
  const h = makeBulkAction({
    action: "squadRetry",
    squads: { d1: { label: "Done One", state: "done" } },
    promptResult: "nope",
  });
  await h.bulkRetrySquads(["d1"]);
  assert.deepEqual(h.calls.posts, []);
});

test("bulk retry uses a plain confirmation naming all squads when none succeeded", async () => {
  const h = makeBulkAction({
    action: "squadRetry",
    squads: { f1: { label: "F1", state: "failed" }, c1: { label: "C1", state: "cancelled" } },
  });
  await h.bulkRetrySquads(["f1", "c1"]);
  assert.equal(h.calls.confirmTexts.length, 1);
  assert.match(h.calls.confirmTexts[0], /Retry 2 squad\(s\)/);
  assert.match(h.calls.confirmTexts[0], /F1/);
  assert.match(h.calls.confirmTexts[0], /C1/);
  assert.deepEqual(h.calls.posts, ["/api/squads/f1/retry", "/api/squads/c1/retry"]);
  assert.deepEqual(h.calls.notifications, [{ kind: "success", msg: "Retried 2 squad(s)." }]);
});

test("bulk retry does nothing when the confirmation is declined", async () => {
  const h = makeBulkAction({
    action: "squadRetry",
    squads: { f1: { state: "failed" } },
    confirmResult: false,
  });
  await h.bulkRetrySquads(["f1"]);
  assert.deepEqual(h.calls.posts, []);
});

// ---------- bulkCancelReviews (shipped review-menu bulk action) ----------

test("bulk review cancel cancels only cancellable reviews, once each, and reports skips", async () => {
  const h = makeBulkAction({
    action: "reviewCancel",
    guardians: { g1: { id: "g1", name: "R1", status: "collecting" }, g2: { id: "g2", name: "R2", status: "done" }, g3: { id: "g3", name: "R3", status: "merging" } },
  });
  await h.bulkCancelReviews(["g1", "g2", "g3"]);
  assert.deepEqual(h.calls.posts, ["/api/guardians/g1/cancel", "/api/guardians/g3/cancel"]);
  assert.ok(h.calls.notifications.some((n) => n.msg.includes("Skipped 1 of the selected review") && n.msg.includes("not in a cancellable status")));
  assert.equal(h.calls.ticks, 1);
});

test("bulk review cancel reports success when every selected review was cancellable", async () => {
  const h = makeBulkAction({
    action: "reviewCancel",
    guardians: { g1: { id: "g1", name: "R1", status: "collecting" }, g2: { id: "g2", name: "R2", status: "in_review" } },
  });
  await h.bulkCancelReviews(["g1", "g2"]);
  assert.deepEqual(h.calls.notifications, [{ kind: "success", msg: "Cancelled 2 review(s)." }]);
});

test("bulk review cancel reports a per-item failure without blocking the others", async () => {
  const h = makeBulkAction({
    action: "reviewCancel",
    guardians: { g1: { id: "g1", name: "R1", status: "collecting" }, g2: { id: "g2", name: "R2", status: "merging" } },
    failIds: ["g1"],
  });
  await h.bulkCancelReviews(["g1", "g2"]);
  assert.deepEqual(h.calls.posts, ["/api/guardians/g1/cancel", "/api/guardians/g2/cancel"]);
  assert.deepEqual(h.calls.notifications, [{ kind: "error", msg: "R1: daemon said no" }]);
});

test("bulk review cancel aborts without posting when the confirmation is declined", async () => {
  const h = makeBulkAction({
    action: "reviewCancel",
    guardians: { g1: { id: "g1", name: "R1", status: "collecting" } },
    confirmResult: false,
  });
  await h.bulkCancelReviews(["g1"]);
  assert.deepEqual(h.calls.posts, []);
});

// ---------- bulkReopenReviews (shipped review-menu bulk action) ----------

test("bulk review reopen reopens only cancelled reviews, once each, and reports skips", async () => {
  const h = makeBulkAction({
    action: "reviewReopen",
    guardians: { g1: { id: "g1", name: "R1", status: "cancelled" }, g2: { id: "g2", name: "R2", status: "collecting" } },
  });
  await h.bulkReopenReviews(["g1", "g2"]);
  assert.deepEqual(h.calls.posts, ["/api/guardians/g1/reopen"]);
  assert.ok(h.calls.notifications.some((n) => n.msg.includes("Skipped 1 of the selected review") && n.msg.includes("not cancelled")));
});

test("bulk review reopen names every review in its confirmation and reports success", async () => {
  const h = makeBulkAction({
    action: "reviewReopen",
    guardians: { g1: { id: "g1", name: "R1", status: "cancelled" }, g2: { id: "g2", name: "R2", status: "cancelled" } },
  });
  await h.bulkReopenReviews(["g1", "g2"]);
  assert.match(h.calls.confirmTexts[0], /Reopen 2 cancelled review\(s\)/);
  assert.match(h.calls.confirmTexts[0], /R1/);
  assert.match(h.calls.confirmTexts[0], /R2/);
  assert.deepEqual(h.calls.posts, ["/api/guardians/g1/reopen", "/api/guardians/g2/reopen"]);
  assert.deepEqual(h.calls.notifications, [{ kind: "success", msg: "Reopened 2 review(s)." }]);
});

// ---------- bulkDeleteReviews (shipped review-menu bulk action) ----------

test("bulk review delete confirms naming every review, then deletes each once", async () => {
  const h = makeBulkAction({
    action: "reviewDelete",
    guardians: { g1: { id: "g1", name: "R1" }, g2: { id: "g2", name: "R2" } },
    guardianMultiSel: new Set(["g1", "g2"]),
  });
  await h.bulkDeleteReviews(["g1", "g2"]);
  assert.match(h.calls.confirmTexts[0], /Delete 2 review\(s\)/);
  assert.match(h.calls.confirmTexts[0], /R1/);
  assert.match(h.calls.confirmTexts[0], /R2/);
  assert.deepEqual(h.calls.dels, ["/api/guardians/g1", "/api/guardians/g2"]);
  assert.deepEqual(h.calls.notifications, [{ kind: "success", msg: "Deleted 2 review(s)." }]);
  assert.equal(h.state().guardianMultiSelSize, 0);
});

test("bulk review delete reports a per-item failure without blocking the others", async () => {
  const h = makeBulkAction({
    action: "reviewDelete",
    guardians: { g1: { id: "g1", name: "R1" }, g2: { id: "g2", name: "R2" } },
    failIds: ["g1"],
  });
  await h.bulkDeleteReviews(["g1", "g2"]);
  assert.deepEqual(h.calls.dels, ["/api/guardians/g1", "/api/guardians/g2"]);
  assert.deepEqual(h.calls.notifications, [{ kind: "error", msg: "R1: daemon said no" }]);
  assert.equal(h.state().guardianMultiSelSize, 0);
});

// ---------- bulkDelete (shipped squad bulk action) ----------

test("bulk squad delete confirms naming every selected squad, then deletes each once", async () => {
  const h = makeBulkAction({
    action: "squadDelete",
    squads: { d1: { label: "Delta One" }, d2: { label: "Delta Two" } },
    multiSel: new Set(["d1", "d2"]),
  });
  await h.bulkDelete();
  assert.match(h.calls.confirmTexts[0], /Delete 2 squad\(s\)/);
  assert.match(h.calls.confirmTexts[0], /Delta One/);
  assert.match(h.calls.confirmTexts[0], /Delta Two/);
  assert.deepEqual(h.calls.dels, ["/api/squads/d1", "/api/squads/d2"]);
  assert.deepEqual(h.calls.notifications, [{ kind: "success", msg: "Deleted 2 squad(s)." }]);
  const st = h.state();
  assert.equal(st.multiSelSize, 0);
  assert.equal(st.selectedSquadId, null);
});

test("bulk squad delete declined posts nothing and keeps the selection", async () => {
  const h = makeBulkAction({
    action: "squadDelete",
    squads: { d1: { label: "D1" } },
    multiSel: new Set(["d1"]),
    confirmResult: false,
  });
  await h.bulkDelete();
  assert.deepEqual(h.calls.dels, []);
  assert.equal(h.state().multiSelSize, 1);
});

test("bulk squad delete reports a per-item failure without blocking the others", async () => {
  const h = makeBulkAction({
    action: "squadDelete",
    squads: { d1: { label: "D1" }, d2: { label: "D2" } },
    multiSel: new Set(["d1", "d2"]),
    failIds: ["d1"],
  });
  await h.bulkDelete();
  assert.deepEqual(h.calls.dels, ["/api/squads/d1", "/api/squads/d2"]);
  assert.deepEqual(h.calls.notifications, [{ kind: "error", msg: "D1: daemon said no" }]);
  assert.equal(h.state().multiSelSize, 0);
});

// ---------- showBulkCancelPreview + confirmBulkCancelSquads (shipped squad bulk cancel) ----------

test("bulk cancel preview lists every affected squad name before confirming", async () => {
  const h = makeBulkAction({
    action: "bulkCancelPreview",
    cancelPreviews: {
      s1: [{ id: "s1", label: "S One" }, { id: "down1", label: "Down One" }],
      s2: [{ id: "s2", label: "S Two" }, { id: "down1", label: "Down One" }],
    },
  });
  await h.showBulkCancelPreview(["s1", "s2"]);
  assert.ok(h.calls.modalHtml.includes("Cancel 2 squads"));
  assert.ok(h.calls.modalHtml.includes("S One"));
  assert.ok(h.calls.modalHtml.includes("S Two"));
  assert.ok(h.calls.modalHtml.includes("Down One"));
  // the deduped cascade appears once even though both selections pull it in
  assert.equal(h.calls.modalHtml.split("Down One").length - 1, 1);
  assert.ok(h.calls.modalHtml.includes('data-click="confirmBulkCancelSquads"'));
  assert.ok(h.calls.modalHtml.includes('data-squad-ids="s1,s2"'));
});

test("bulk cancel preview failure computes nothing and shows an error", async () => {
  const h = makeBulkAction({ action: "bulkCancelPreview", failIds: ["s1"] });
  await h.showBulkCancelPreview(["s1", "s2"]);
  assert.deepEqual(h.calls.notifications, [{ kind: "error", msg: "Failed to compute cancel preview: daemon said no." }]);
  assert.equal(h.calls.modalHtml, undefined);
});

test("bulk cancel confirm posts once per squad and reports per-item failures", async () => {
  const h = makeBulkAction({
    action: "bulkCancelConfirm",
    squads: { s1: { label: "S1" }, s2: { label: "S2" }, s3: { label: "S3" } },
    failIds: ["s2"],
  });
  await h.confirmBulkCancelSquads("s1,s2,s3");
  assert.deepEqual(h.calls.posts, ["/api/squads/s1/cancel", "/api/squads/s2/cancel", "/api/squads/s3/cancel"]);
  assert.deepEqual(h.calls.notifications, [{ kind: "error", msg: "S2: daemon said no" }]);
});

test("bulk cancel confirm reports success when every squad cancelled", async () => {
  const h = makeBulkAction({ action: "bulkCancelConfirm", squads: { s1: { label: "S1" } } });
  await h.confirmBulkCancelSquads("s1");
  assert.deepEqual(h.calls.notifications, [{ kind: "success", msg: "Cancelled 1 squad(s)." }]);
});

// ---------- confirmBulkRestart (shipped squad bulk restart) ----------

test("bulk restart confirm posts once per squad and reports per-item failures", async () => {
  const h = makeBulkAction({
    action: "bulkRestartConfirm",
    squads: { s1: { label: "S1" }, s2: { label: "S2" } },
    failIds: ["s1"],
  });
  await h.confirmBulkRestart("s1,s2");
  assert.deepEqual(h.calls.posts, ["/api/squads/s1/restart", "/api/squads/s2/restart"]);
  assert.deepEqual(h.calls.notifications, [{ kind: "error", msg: "S1: daemon said no" }]);
});

test("bulk restart confirm forwards the modal's restart note to every squad", async () => {
  const h = makeBulkAction({
    action: "bulkRestartConfirm",
    squads: { s1: { label: "S1" }, s2: { label: "S2" } },
    note: "redo these",
    applyAllToAll: true,
  });
  await h.confirmBulkRestart("s1,s2");
  assert.deepEqual(h.calls.postBodies, [{ note: "redo these", apply_to_all: true }, { note: "redo these", apply_to_all: true }]);
  assert.deepEqual(h.calls.notifications, [{ kind: "success", msg: "Restarted 2 squad(s)." }]);
});

test("bulk restart confirm without a note sends no body", async () => {
  const h = makeBulkAction({ action: "bulkRestartConfirm", squads: { s1: { label: "S1" } } });
  await h.confirmBulkRestart("s1");
  assert.deepEqual(h.calls.postBodies, [undefined]);
});

// ---------- hide/unhide routing (squad + review menu items) ----------

test("the squad hide menu item applies the whole selection only when the clicked squad is in it", async () => {
  const h = makeBulkAction({ action: "squadHideRoute", multiSel: new Set(["s1", "s2"]) });
  await h.setSquadHiddenFromMenu("s1", true);
  assert.deepEqual(h.calls.bulkCalls, ["bulkHideSquads"]);
  assert.deepEqual(h.calls.singleHides, []);
  // fallback: a clicked row outside the selection hides just that row
  await h.setSquadHiddenFromMenu("s9", true);
  assert.deepEqual(h.calls.bulkCalls, ["bulkHideSquads"]);
  assert.deepEqual(h.calls.singleHides, [["s9", true]]);
  // unhide routes the same way
  await h.setSquadHiddenFromMenu("s2", false);
  assert.deepEqual(h.calls.bulkCalls, ["bulkHideSquads", "bulkUnhideSquads"]);
});

test("the review hide menu item applies the whole selection only when the clicked review is in it", async () => {
  const h = makeBulkAction({ action: "reviewHide", guardianMultiSel: new Set(["g1", "g2"]) });
  await h.setReviewHiddenFromMenu("g1", true);
  assert.deepEqual(h.calls.posts, ["/api/hidden/reviews/g1", "/api/hidden/reviews/g2"]);
  assert.deepEqual(h.calls.notifications, [{ kind: "success", msg: "2 review(s) hidden." }]);
  // fallback: a clicked row outside the selection hides just that row
  await h.setReviewHiddenFromMenu("g9", true);
  assert.deepEqual(h.calls.posts, ["/api/hidden/reviews/g1", "/api/hidden/reviews/g2"]);
  assert.deepEqual(h.calls.singleHides, [["g9", true]]);
});

test("bulk review unhide deletes once per selected review and reports success", async () => {
  const h = makeBulkAction({ action: "reviewUnhide", guardianMultiSel: new Set(["g1", "g2"]) });
  await h.bulkUnhideReviews();
  assert.deepEqual(h.calls.dels, ["/api/hidden/reviews/g1", "/api/hidden/reviews/g2"]);
  assert.deepEqual(h.calls.notifications, [{ kind: "success", msg: "2 review(s) unhidden." }]);
});

test("bulk review hide reports a per-item failure without blocking the others", async () => {
  const h = makeBulkAction({ action: "reviewHide", guardianMultiSel: new Set(["g1", "g2"]), failIds: ["g1"] });
  await h.bulkHideReviews();
  assert.deepEqual(h.calls.posts, ["/api/hidden/reviews/g1", "/api/hidden/reviews/g2"]);
  assert.deepEqual(h.calls.notifications, [{ kind: "error", msg: "g1: daemon said no" }]);
});

// ---------- popup-bearing menu items are disabled under a multi-selection ----------

test("a multi-selection disables the squad menu's popup-bearing items (Rename, Watch, Logs)", () => {
  const menu = makeSquadMenu({
    squads: { s1: { label: "S1", state: "done" } },
    selection: ["s1", "s2"],
    clickedId: "s1",
  });
  const html = menu.innerHTML;
  assert.equal((html.match(/ctx-disabled/g) || []).length, 3);
  assert.ok(html.includes("✎ Rename"));
  assert.ok(html.includes("◎ Watch"));
  assert.ok(html.includes("📄 Logs"));
  assert.ok(!html.includes('data-click="renameSquad"'));
  assert.ok(!html.includes('data-click="toggleWatch"'));
  assert.ok(!html.includes('data-click="openLogsFromSquadMenu"'));
  // every applicable bulk action stays clickable
  for (const handler of ["deleteSquad", "retrySquad", "restartSquad", "cancelSquad", "openStatusPickerForSquadMenuItem", "hideSquadMenuItem", "unhideSquadMenuItem"]) {
    assert.ok(html.includes(`data-click="${handler}"`), `expected ${handler} to stay clickable`);
  }
});

test("a single-selection squad menu keeps Rename/Watch/Logs clickable", () => {
  const menu = makeSquadMenu({
    squads: { s1: { label: "S1", state: "done" } },
    selection: ["s1"],
    clickedId: "s1",
  });
  const html = menu.innerHTML;
  assert.ok(!html.includes("ctx-disabled"));
  assert.ok(html.includes('data-click="renameSquad"'));
  assert.ok(html.includes('data-click="toggleWatch"'));
  assert.ok(html.includes('data-click="openLogsFromSquadMenu"'));
});

test("a multi-selection disables the review menu's popup-bearing items (Edit Details, Watch)", () => {
  const menu = makeReviewMenu({
    guardians: { g1: { id: "g1", name: "R1", status: "collecting" } },
    selection: ["g1", "g2"],
    clickedId: "g1",
  });
  const html = menu.innerHTML;
  assert.equal((html.match(/ctx-disabled/g) || []).length, 2);
  assert.ok(html.includes("✎ Edit Details"));
  assert.ok(html.includes("◎ Watch"));
  assert.ok(!html.includes('data-click="openEditReviewDetailsFromMenu"'));
  assert.ok(!html.includes('data-click="toggleWatch"'));
  // every applicable bulk action stays clickable
  for (const handler of ["deleteReview", "cancelReview", "hideReviewMenuItem", "unhideReviewMenuItem"]) {
    assert.ok(html.includes(`data-click="${handler}"`), `expected ${handler} to stay clickable`);
  }
});

test("a single-selection review menu keeps Edit Details/Watch clickable", () => {
  const menu = makeReviewMenu({
    guardians: { g1: { id: "g1", name: "R1", status: "collecting" } },
    selection: ["g1"],
    clickedId: "g1",
  });
  const html = menu.innerHTML;
  assert.ok(!html.includes("ctx-disabled"));
  assert.ok(html.includes('data-click="openEditReviewDetailsFromMenu"'));
  assert.ok(html.includes('data-click="toggleWatch"'));
});

// ---------- set-status over a multi-selection (RAL-508) ----------

test("the squad menu's Set Status routes a multi-selection into the bulk picker, one item per selected squad", () => {
  const h = makeStatusRoute({
    squads: { s1: { label: "Alpha" }, s2: { label: "Beta" } },
    selection: ["s1", "s2"],
    clickedId: "s1",
  });
  assert.equal(h.bulkCalled, 1);
  assert.equal(h.singlePickerId, undefined);
  assert.deepEqual(h.pickerItems, [
    { squadId: "s1", label: "Alpha" },
    { squadId: "s2", label: "Beta" },
  ]);
});

test("the squad menu's Set Status falls back to the single-squad picker when the clicked squad is not in the selection", () => {
  const h = makeStatusRoute({
    squads: { s1: { label: "Alpha" }, s9: { label: "Outsider" } },
    selection: ["s1", "s2"],
    clickedId: "s9",
  });
  assert.equal(h.bulkCalled, 0);
  assert.equal(h.singlePickerId, "s9");
  assert.equal(h.pickerItems, undefined);
});

test("bulk set-status opens the picker scoped to every selected squad and does nothing for an empty selection", async () => {
  const h = makeBulkAction({
    action: "bulkSetStatus",
    squads: { s1: { label: "Alpha" }, s2: { label: "Beta" } },
    multiSel: new Set(["s1", "s2"]),
  });
  await h.bulkSetStatus({ stopPropagation() {} });
  assert.deepEqual(h.calls.pickerItems, [
    { squadId: "s1", kind: "squad", taskIdx: 0, cellIdx: -1, proofIdx: -1, proofScope: "", label: "Alpha" },
    { squadId: "s2", kind: "squad", taskIdx: 0, cellIdx: -1, proofIdx: -1, proofScope: "", label: "Beta" },
  ]);
  assert.deepEqual(h.calls.posts, []);

  const empty = makeBulkAction({ action: "bulkSetStatus", multiSel: new Set() });
  await empty.bulkSetStatus({ stopPropagation() {} });
  assert.equal(empty.calls.pickerItems, undefined);
});

test("bulk set-status applies the chosen state to every selected squad exactly once and reports success", async () => {
  const h = makeBulkAction({
    action: "setStatusPick",
    statusItems: [
      { squadId: "s1", kind: "squad", taskIdx: 0, cellIdx: -1, proofIdx: -1, proofScope: "", label: "Alpha" },
      { squadId: "s2", kind: "squad", taskIdx: 0, cellIdx: -1, proofIdx: -1, proofScope: "", label: "Beta" },
    ],
  });
  await h.doPickStatus("paused");
  assert.deepEqual(h.calls.posts, ["/api/squads/s1/set-status", "/api/squads/s2/set-status"]);
  assert.deepEqual(h.calls.postBodies, [
    { kind: "squad", task_idx: 0, cell_idx: -1, proof_idx: -1, proof_scope: "", state: "paused" },
    { kind: "squad", task_idx: 0, cell_idx: -1, proof_idx: -1, proof_scope: "", state: "paused" },
  ]);
  assert.deepEqual(h.calls.notifications, [{ kind: "success", msg: 'Set 2 squad(s) to "paused".' }]);
});

test("bulk set-status reports a per-item failure without blocking the rest of the selection", async () => {
  const h = makeBulkAction({
    action: "setStatusPick",
    statusItems: [
      { squadId: "s1", kind: "squad", taskIdx: 0, cellIdx: -1, proofIdx: -1, proofScope: "", label: "Alpha" },
      { squadId: "s2", kind: "squad", taskIdx: 0, cellIdx: -1, proofIdx: -1, proofScope: "", label: "Beta" },
      { squadId: "s3", kind: "squad", taskIdx: 0, cellIdx: -1, proofIdx: -1, proofScope: "", label: "Gamma" },
    ],
    failIds: ["s2"],
  });
  await h.doPickStatus("paused");
  assert.deepEqual(h.calls.posts, [
    "/api/squads/s1/set-status",
    "/api/squads/s2/set-status",
    "/api/squads/s3/set-status",
  ]);
  assert.equal(h.calls.notifications.length, 1);
  assert.match(h.calls.notifications[0].kind ? h.calls.notifications[0].msg : "", /Set 2 of 3 squad\(s\) to "paused"/);
  assert.match(h.calls.notifications[0].msg, /Failures: Beta: daemon said no/);
});

test("bulk set-status names every selected squad in its confirmation before applying", async () => {
  const h = makeBulkAction({
    action: "setStatusPick",
    statusItems: [
      { squadId: "s1", kind: "squad", taskIdx: 0, cellIdx: -1, proofIdx: -1, proofScope: "", label: "Alpha" },
      { squadId: "s2", kind: "squad", taskIdx: 0, cellIdx: -1, proofIdx: -1, proofScope: "", label: "Beta" },
    ],
    confirmResult: false,
  });
  await h.doPickStatus("done");
  assert.match(h.calls.confirmTexts[0], /Set 2 items to "done"/);
  assert.match(h.calls.confirmTexts[0], /• Alpha/);
  assert.match(h.calls.confirmTexts[0], /• Beta/);
  assert.match(h.calls.confirmTexts[0], /This cannot be undone\./);
  assert.deepEqual(h.calls.posts, []);
});

// ---------- right-click menu routing: whole selection vs. clicked row ----------

test("right-clicking a selected squad and choosing Retry routes the whole selection into the bulk retry", async () => {
  const h = await makeSquadMenuRoute({ region: "squadRetryRoute", selection: ["s1", "s2"], clickedId: "s1" });
  assert.deepEqual(h.bulk, [["bulkRetrySquads", ["s1", "s2"]]]);
  assert.deepEqual(h.single, []);
  assert.deepEqual(h.posts, []);
});

test("right-clicking a selected squad and choosing Restart routes the whole selection into the bulk restart", async () => {
  const h = await makeSquadMenuRoute({ region: "squadRestartRoute", selection: ["s1", "s2"], clickedId: "s2" });
  assert.deepEqual(h.bulk, [["bulkRestartSquads", ["s1", "s2"]]]);
  assert.deepEqual(h.single, []);
});

test("right-clicking a selected squad and choosing Cancel routes the whole selection into the bulk cascade preview", async () => {
  const h = await makeSquadMenuRoute({ region: "squadCancelRoute", selection: ["s1", "s2"], clickedId: "s1" });
  assert.deepEqual(h.bulk, [["showBulkCancelPreview", ["s1", "s2"]]]);
  assert.deepEqual(h.single, []);
});

test("right-clicking a selected squad and choosing Delete routes the whole selection into the bulk delete", async () => {
  const h = await makeSquadMenuRoute({ region: "squadDeleteRoute", selection: ["s1", "s2"], clickedId: "s1" });
  assert.deepEqual(h.bulk, [["bulkDelete"]]);
  assert.deepEqual(h.dels, []);
  assert.deepEqual(h.confirms, []);
});

test("a right-click on a squad outside the selection falls back to that row alone for each menu action", async () => {
  for (const [region, singleUrl, bulkName] of [
    ["squadRetryRoute", "/api/squads/s9/retry", "bulkRetrySquads"],
    ["squadRestartRoute", "/api/squads/s9/restart/preview", "bulkRestartSquads"],
    ["squadCancelRoute", "/api/squads/s9/cancel/preview", "showBulkCancelPreview"],
  ]) {
    const h = await makeSquadMenuRoute({ region, squads: { s9: { label: "Outsider", state: "done" } }, selection: ["s1", "s2"], clickedId: "s9" });
    assert.deepEqual(h.bulk, [], `${region}: no bulk call for an unselected row`);
    const applied = region === "squadRetryRoute" ? h.posts : h.single.map((e) => e[1]);
    assert.deepEqual(applied, [singleUrl], `${region}: single-item path used the clicked row`);
    assert.ok(!h.bulk.some(([name]) => name === bulkName));
  }
  const del = await makeSquadMenuRoute({ region: "squadDeleteRoute", squads: { s9: { label: "Outsider" } }, selection: ["s1", "s2"], clickedId: "s9", confirmResult: true });
  assert.deepEqual(del.bulk, []);
  assert.deepEqual(del.dels, ["/api/squads/s9"]);
  assert.equal(del.confirms.length, 1);
});

test("a right-click on a squad outside the selection never triggers any bulk action even when the row is unknown", async () => {
  for (const region of ["squadRetryRoute", "squadRestartRoute", "squadCancelRoute", "squadDeleteRoute"]) {
    const h = await makeSquadMenuRoute({ region, selection: ["s1", "s2"], clickedId: "s9" });
    assert.deepEqual(h.bulk, [], `${region}: unknown row must not widen into the selection`);
  }
});
