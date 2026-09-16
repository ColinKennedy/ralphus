// RAL-421: the Triage threshold editor's preview -> confirm -> cancel flow.
// The flow is the same at both editor entry points (the Triage tab's pool
// table and the Projects tab's auto-review-thresholds popup): typed input
// never mutates anything; a non-mutating `/preview` fetch populates a
// Confirm/Cancel line; Confirm posts the *previewed* threshold (never a
// re-read of the input) which persists it and drains the pool in
// threshold-sized batches; Cancel discards the preview with zero requests.
// See ./board-triage-confirm.mjs for how the shipped code is sliced out.

import test from "node:test";
import assert from "node:assert/strict";
import { makeTriageConfirm, makeRowEvent, DRAINING_PREVIEW, DRAINABLE_POOL } from "./board-triage-confirm.mjs";

// ---- pure helpers (shared by both editor entry points) ----

test("triagePreviewKey is distinct per (project, triage_type) pair", () => {
  const { triagePreviewKey } = makeTriageConfirm();
  assert.equal(triagePreviewKey("proj", "bug"), triagePreviewKey("proj", "bug"));
  assert.notEqual(triagePreviewKey("proj", "bug"), triagePreviewKey("proj::core", "bug"));
  assert.notEqual(triagePreviewKey("proj", "bug"), triagePreviewKey("proj", "feature"));
});

test("preview effect for a draining pool spells out batches and remainder", () => {
  const { triageThresholdPreviewEffect } = makeTriageConfirm();
  const effect = triageThresholdPreviewEffect(DRAINING_PREVIEW);
  assert.ok(effect.includes("threshold to 3"), effect);
  assert.ok(effect.includes("6 cell(s) drain now as 2 review(s)"), effect);
  assert.ok(effect.includes("1 cell(s) stay pooled"), effect);
});

test("preview effect for a pool below the threshold says no review is created", () => {
  const { triageThresholdPreviewEffect } = makeTriageConfirm();
  const effect = triageThresholdPreviewEffect({
    project: "proj", triage_type: "bug", proposed_threshold: 5, clearing: false,
    pooled: 2, full_batches: 0, cells_drained: 0, cells_left: 2,
  });
  assert.ok(effect.includes("no review is created now"), effect);
});

test("preview effect for a clear says the trigger is removed without draining", () => {
  const { triageThresholdPreviewEffect } = makeTriageConfirm();
  const effect = triageThresholdPreviewEffect({
    project: "proj", triage_type: "bug", proposed_threshold: null, clearing: true,
    pooled: 7, full_batches: 0, cells_drained: 0, cells_left: 7,
  });
  assert.ok(effect.includes("clears this pool's count trigger"), effect);
  assert.ok(effect.includes("no review is created"), effect);
});

test("confirm line renders Confirm and Cancel wired to the previewed key", () => {
  const { triageThresholdConfirmLine } = makeTriageConfirm();
  const line = triageThresholdConfirmLine(DRAINING_PREVIEW);
  assert.ok(line.includes("data-click=\"confirmPoolThreshold\""), line);
  assert.ok(line.includes("data-click=\"cancelPoolThresholdPreview\""), line);
  assert.ok(line.includes("data-project=\"proj\""), line);
  assert.ok(line.includes("data-triage-type=\"bug\""), line);
});

test("confirm line can point a second editor entry point at its own handlers", () => {
  const { triageThresholdConfirmLine } = makeTriageConfirm();
  const line = triageThresholdConfirmLine(DRAINING_PREVIEW, "confirmProjectTriageThreshold", "cancelProjectTriageThresholdPreview");
  assert.ok(line.includes("data-click=\"confirmProjectTriageThreshold\""), line);
  assert.ok(line.includes("data-click=\"cancelProjectTriageThresholdPreview\""), line);
});

// ---- Triage-tab handler flow (entry point 1) ----

test("preview pools the row's input and never touches the confirm route", async () => {
  const api = makeTriageConfirm();
  const { previewPoolThreshold } = api;
  const e = makeRowEvent("  3  ");
  await previewPoolThreshold(e, "proj", "bug");

  assert.equal(api.calls.fetches.length, 1, "preview is exactly one request");
  const [req] = api.calls.fetches;
  assert.equal(req.url, "/api/triage/pools/threshold/preview");
  assert.equal(req.init.method, "POST");
  assert.deepEqual(JSON.parse(req.init.body), { project: "proj", triage_type: "bug", threshold: 3 });
  // The preview response is now driving the Confirm/Cancel line.
  assert.deepEqual(api.previews()[api.triagePreviewKey("proj", "bug")], DRAINING_PREVIEW);
  assert.equal(api.calls.pollTriage, 0, "preview must not re-poll");
  assert.equal(api.calls.renderTriage, 1, "preview re-renders to show the Confirm line");
});

test("preview maps a blank input to threshold null (a clear preview)", async () => {
  const api = makeTriageConfirm();
  const { previewPoolThreshold } = api;
  await previewPoolThreshold(makeRowEvent("  "), "proj", "bug");
  assert.equal(api.calls.fetches.length, 1);
  assert.deepEqual(
    JSON.parse(api.calls.fetches[0].init.body),
    { project: "proj", triage_type: "bug", threshold: null },
  );
});

test("preview rejects a malformed threshold without any request", async () => {
  const api = makeTriageConfirm();
  const { previewPoolThreshold } = api;
  await previewPoolThreshold(makeRowEvent("0"), "proj", "bug");
  assert.equal(api.calls.fetches.length, 0, "an invalid threshold must never reach the daemon");
  assert.equal(api.triageError(), "Threshold must be a whole number of at least 1, or blank to clear it.");
});

test("confirm posts the previewed threshold, not the drifted input", async () => {
  const api = makeTriageConfirm();
  const { previewPoolThreshold, confirmPoolThreshold } = api;
  await previewPoolThreshold(makeRowEvent("3"), "proj", "bug");

  // The input drifts after preview -- a human edits the box again. Confirm
  // must still fire the exact previewed value, never this new one.
  await confirmPoolThreshold("proj", "bug");

  assert.equal(api.calls.fetches.length, 2);
  const [previewReq, confirmReq] = api.calls.fetches;
  assert.equal(previewReq.url, "/api/triage/pools/threshold/preview");
  assert.equal(confirmReq.url, "/api/triage/pools/threshold");
  assert.deepEqual(JSON.parse(confirmReq.init.body), { project: "proj", triage_type: "bug", threshold: 3 });
  assert.equal(api.calls.pollTriage, 1, "confirm re-polls to refresh pool state");
  assert.equal(api.previews()[api.triagePreviewKey("proj", "bug")], undefined, "confirm clears the preview");
});

test("confirm with no preview is a no-op (defense in depth)", async () => {
  const api = makeTriageConfirm();
  const { confirmPoolThreshold } = api;
  await confirmPoolThreshold("proj", "bug");
  assert.equal(api.calls.fetches.length, 0);
});

test("cancel discards the preview with zero requests", async () => {
  const api = makeTriageConfirm();
  const { previewPoolThreshold, cancelPoolThresholdPreview } = api;
  await previewPoolThreshold(makeRowEvent("3"), "proj", "bug");
  const before = api.calls.fetches.length;
  cancelPoolThresholdPreview("proj", "bug");
  assert.equal(api.calls.fetches.length, before, "cancel never talks to the daemon");
  assert.equal(api.previews()[api.triagePreviewKey("proj", "bug")], undefined);
  assert.equal(api.calls.renderTriage, 2, "cancel re-renders to drop the Confirm line");
});

// ---- RAL-449 manual "Drain now" flow ----

test("drain confirm line names the candidate count and the bypassed threshold", () => {
  const { triageDrainConfirmLine } = makeTriageConfirm();
  const line = triageDrainConfirmLine({ project: "proj", triage_type: "bug", count: 7, threshold: 3 });
  assert.ok(line.includes("7 candidate(s)"), line);
  assert.ok(line.includes("the configured threshold of 3"), line);
  assert.ok(line.includes("data-click=\"confirmDrainTriagePool\""), line);
  assert.ok(line.includes("data-click=\"cancelDrainTriagePool\""), line);
  assert.ok(line.includes("data-project=\"proj\""), line);
  assert.ok(line.includes("data-triage-type=\"bug\""), line);
});

test("drain confirm line says no threshold is configured when the pool has none", () => {
  const { triageDrainConfirmLine } = makeTriageConfirm();
  const line = triageDrainConfirmLine({ project: "proj", triage_type: "bug", count: 4, threshold: null });
  assert.ok(line.includes("no threshold is configured"), line);
});

test("requesting a drain captures the row's current pool state without any request", () => {
  const api = makeTriageConfirm();
  const { requestDrainTriagePool } = api;
  requestDrainTriagePool({}, "proj", "bug");
  assert.equal(api.calls.fetches.length, 0, "requesting a drain must never talk to the daemon");
  assert.deepEqual(api.drainConfirms()[api.triagePreviewKey("proj", "bug")], DRAINABLE_POOL);
  assert.equal(api.calls.renderTriage, 1, "requesting a drain re-renders to show the Confirm line");
});

test("requesting a drain on a pool with no eligible candidates is a no-op", () => {
  const api = makeTriageConfirm({ pools: [{ project: "proj", triage_type: "bug", count: 0, threshold: 3 }] });
  const { requestDrainTriagePool } = api;
  requestDrainTriagePool({}, "proj", "bug");
  assert.equal(api.calls.renderTriage, 0);
  assert.equal(api.drainConfirms()[api.triagePreviewKey("proj", "bug")], undefined);
});

test("confirming a drain posts to the drain route and re-polls", async () => {
  const api = makeTriageConfirm();
  const { requestDrainTriagePool, confirmDrainTriagePool } = api;
  requestDrainTriagePool({}, "proj", "bug");
  await confirmDrainTriagePool("proj", "bug");

  assert.equal(api.calls.fetches.length, 1);
  const [req] = api.calls.fetches;
  assert.equal(req.url, "/api/triage/pools/drain");
  assert.equal(req.init.method, "POST");
  assert.deepEqual(JSON.parse(req.init.body), { project: "proj", triage_type: "bug" });
  assert.equal(api.calls.pollTriage, 1, "confirm re-polls to refresh pool state");
  assert.equal(api.drainConfirms()[api.triagePreviewKey("proj", "bug")], undefined, "confirm clears the pending state");
});

test("confirming a drain with no pending request is a no-op (defense in depth)", async () => {
  const api = makeTriageConfirm();
  const { confirmDrainTriagePool } = api;
  await confirmDrainTriagePool("proj", "bug");
  assert.equal(api.calls.fetches.length, 0);
});

test("cancelling a drain discards it with zero requests", () => {
  const api = makeTriageConfirm();
  const { requestDrainTriagePool, cancelDrainTriagePool } = api;
  requestDrainTriagePool({}, "proj", "bug");
  const before = api.calls.fetches.length;
  cancelDrainTriagePool("proj", "bug");
  assert.equal(api.calls.fetches.length, before, "cancel never talks to the daemon");
  assert.equal(api.drainConfirms()[api.triagePreviewKey("proj", "bug")], undefined);
  assert.equal(api.calls.renderTriage, 2, "cancel re-renders to drop the Confirm line");
});