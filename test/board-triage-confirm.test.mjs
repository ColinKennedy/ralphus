// RAL-421: the shared threshold-preview helpers the Projects tab's
// auto-review-thresholds popup renders its Confirm/Cancel line with, and
// RAL-449: the Triage tab details pane's "Drain now" request -> confirm ->
// cancel flow. See ./board-triage-confirm.mjs for how the shipped code is
// sliced out.

import test from "node:test";
import assert from "node:assert/strict";
import { makeTriageConfirm, DRAINING_PREVIEW, DRAINABLE_POOL } from "./board-triage-confirm.mjs";

// ---- pure preview helpers (the Projects-tab popup) ----

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
  const line = triageThresholdConfirmLine(DRAINING_PREVIEW, "confirmProjectTriageThreshold", "cancelProjectTriageThresholdPreview");
  assert.ok(line.includes("data-project=\"proj\""), line);
  assert.ok(line.includes("data-triage-type=\"bug\""), line);
});

test("confirm line wires Confirm and Cancel to the handlers it is given", () => {
  const { triageThresholdConfirmLine } = makeTriageConfirm();
  const line = triageThresholdConfirmLine(DRAINING_PREVIEW, "confirmProjectTriageThreshold", "cancelProjectTriageThresholdPreview");
  assert.ok(line.includes("data-click=\"confirmProjectTriageThreshold\""), line);
  assert.ok(line.includes("data-click=\"cancelProjectTriageThresholdPreview\""), line);
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

test("requesting a drain captures the pool's current state without any request", () => {
  const api = makeTriageConfirm();
  const { requestDrainTriagePool } = api;
  requestDrainTriagePool({}, "proj", "bug");
  assert.equal(api.calls.fetches.length, 0, "requesting a drain must never talk to the daemon");
  assert.deepEqual(api.drainConfirms()[api.triagePreviewKey("proj", "bug")], DRAINABLE_POOL);
  assert.equal(api.calls.renderTriage, 1, "requesting a drain re-renders to show the Confirm line");
  assert.equal(api.selKey(), api.triagePreviewKey("proj", "bug"), "requesting a drain selects that pool so its Confirm line is visible");
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