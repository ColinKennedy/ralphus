// Hash round-trip for the Reviews tab's branch / detail tab / Live / log-drawer params.
import test from "node:test";
import assert from "node:assert/strict";
import { inspectorHash } from "./board-review-inspector-hash.mjs";

const { defaultInspectorHashState, encodeInspectorHash, decodeInspectorHash } = inspectorHash;

/** @param {object} over */
const roundTrip = (over) => {
  const p = new URLSearchParams();
  encodeInspectorHash(p, { ...defaultInspectorHashState(), ...over });
  return { qs: p.toString(), state: decodeInspectorHash(new URLSearchParams(p.toString())) };
};

test("the default state writes no params", () => {
  assert.equal(roundTrip({}).qs, "");
});

test("branch and tab round-trip", () => {
  const { qs, state } = roundTrip({ branchId: "b-1", tab: "feedback" });
  assert.equal(qs, "branch=b-1&itab=feedback");
  assert.equal(state.branchId, "b-1");
  assert.equal(state.tab, "feedback");
});

test("Live run and sub-view round-trip", () => {
  const { state } = roundTrip({ branchId: "b-1", tab: "live", runIdx: 2, liveSub: "system" });
  assert.equal(state.tab, "live");
  assert.equal(state.runIdx, 2);
  assert.equal(state.liveSub, "system");
});

test("log drawer open and scope round-trip", () => {
  const { state } = roundTrip({ dockOpen: true, dockScope: "b-9" });
  assert.equal(state.dockOpen, true);
  assert.equal(state.dockScope, "b-9");
});

test("unknown tab and sub-view fall back to defaults", () => {
  const s = decodeInspectorHash(new URLSearchParams("branch=b&itab=bogus&lsub=nope&run=x"));
  assert.equal(s.tab, "overview");
  assert.equal(s.liveSub, "terminal");
  assert.equal(s.runIdx, null);
});

test("inapplicable params are ignored", () => {
  const noBranch = decodeInspectorHash(new URLSearchParams("itab=live&lsub=prompt"));
  assert.equal(noBranch.tab, "overview");
  const notLive = decodeInspectorHash(new URLSearchParams("branch=b&itab=feedback&lsub=prompt&run=1"));
  assert.equal(notLive.liveSub, "terminal");
  assert.equal(notLive.runIdx, null);
});
