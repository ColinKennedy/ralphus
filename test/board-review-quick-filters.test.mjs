// Coverage for RAL-559: the Reviews "Needs you" quick filter keys on the
// daemon-derived `needs_attention` flag, not on status alone. Evaluates the
// real RALPHUS-REVIEW-QUICK-FILTERS region of the shipped board source.
//
// Run with `npm test` (node --test).

import test from "node:test";
import assert from "node:assert/strict";
import { boardScript } from "./board-source.mjs";

const BEGIN = "// RALPHUS-REVIEW-QUICK-FILTERS:BEGIN";
const END = "// RALPHUS-REVIEW-QUICK-FILTERS:END";
const src = boardScript();
const from = src.indexOf(BEGIN);
const to = src.indexOf(END);
assert.ok(from !== -1 && to > from, "quick-filter marker region missing from the board chunks");
// eslint-disable-next-line no-new-func -- evaluating the real shipped source is the point.
const { reviewMatchesQuickFilter: matches } = new Function(
  `let reviewFilters; let guardians;
   ${src.slice(from + BEGIN.length, to)}
   return { reviewMatchesQuickFilter, REVIEW_QUICK_FILTERS };`,
)();

test("needs: an in_review stack still processing is hidden", () => {
  assert.equal(matches("needs", { status: "in_review", needs_attention: false }), false);
  assert.equal(matches("needs", { status: "in_review" }), false);
});

test("needs: a settled in_review stack (all passing, or a failing branch) is shown", () => {
  assert.equal(matches("needs", { status: "in_review", needs_attention: true }), true);
});

test("needs: stalled merges are shown when flagged", () => {
  assert.equal(matches("needs", { status: "merge_failed", needs_attention: true }), true);
  assert.equal(matches("needs", { status: "merge_stopped", needs_attention: true }), true);
});

test("needs: terminal statuses never match, flag or not", () => {
  for (const status of ["merged", "approved", "cancelled", "deployed"]) {
    assert.equal(matches("needs", { status, needs_attention: true }), false, status);
  }
});

test("other presets and no preset ignore the flag", () => {
  assert.equal(matches("active", { status: "in_review", needs_attention: false }), true);
  assert.equal(matches("closed", { status: "merged" }), true);
  assert.equal(matches("", { status: "in_review" }), true);
});
