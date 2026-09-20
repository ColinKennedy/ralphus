// Coverage for RAL-474: the Tasks toolbar's and Reviews sidebar's PR-status
// widgets, rebuilt on top of RAL-475's shared single-select Status dropdown
// instead of a native `<select>` (which used to be destroyed and recreated
// on every poll, closing itself out from under the user mid-click -- see
// ./board-pr-status-filter.mjs's header). These tests drive the real
// production config-building/render/set functions from 15-tasks.js and
// 25-chrome.js fused with the real shared component, so "open, stays open,
// select, closes, filter state updates" is exercised end to end, not just
// at the generic-component level (already covered by status-dropdown.test.mjs).
//
// Run with `npm test` (node --test).

import test from "node:test";
import assert from "node:assert/strict";
import { makeTasksPrStatusFilter, makeReviewsPrStatusFilter } from "./board-pr-status-filter.mjs";

// ---------- Tasks tab ----------

test("tasks: PR-status dropdown defaults to 'any' (off), translated to selectedValue=null for the shared component", () => {
  const d = makeTasksPrStatusFilter();
  const config = d.ttPrStatusDropdownConfig();
  assert.equal(config.id, "tasks-pr");
  assert.equal(config.mode, "single");
  assert.equal(config.selectedValue, null);
  assert.deepEqual(config.options.map((o) => o.value).sort(), ["failing", "passing", "pending"]);
});

test("tasks: PR-status dropdown options carry the documented PR CI color roles", () => {
  const d = makeTasksPrStatusFilter();
  const config = d.ttPrStatusDropdownConfig();
  const byValue = Object.fromEntries(config.options.map((o) => [o.value, o.color]));
  assert.deepEqual(byValue, { passing: "--done", failing: "--failed", pending: "--pending" });
});

test("tasks: clicking the trigger opens the menu and it stays open (not destroyed by a same-tick re-render)", () => {
  const d = makeTasksPrStatusFilter();
  d.renderTtPrStatusFilter();
  d.statusDropdownToggleMenu("tasks-pr", d.fakeClick());
  assert.ok(d.byIdMap.has(d.statusDropdownMenuId("tasks-pr")), "menu must be open after the trigger click");

  // Simulate a poll tick re-rendering the toolbar while the menu is open --
  // this is exactly what destroyed the old native <select> mid-interaction.
  d.renderTtPrStatusFilter();
  assert.ok(d.byIdMap.has(d.statusDropdownMenuId("tasks-pr")), "a same-tick re-render must not close the open menu");
  const trigger = d.byIdMap.get(d.statusDropdownTriggerId("tasks-pr"));
  assert.equal(trigger.getAttribute("aria-expanded"), "true");
});

test("tasks: picking a status closes the menu, updates taskTabFilters.prStatus, and re-renders/syncs the hash", () => {
  const d = makeTasksPrStatusFilter();
  d.renderTtPrStatusFilter();
  d.statusDropdownToggleMenu("tasks-pr", d.fakeClick());
  d.statusDropdownSelectSingle("tasks-pr", "failing");
  assert.equal(d.taskTabFilters.prStatus, "failing");
  assert.ok(!d.byIdMap.has(d.statusDropdownMenuId("tasks-pr")), "picking a status must close the menu");
  assert.equal(d.calls.renderTasksTab, 1);
  assert.equal(d.calls.ttScrollSelectionIntoView, 1);
  assert.equal(d.calls.syncHash, 1);
});

test("tasks: picking 'Any' translates the shared component's null back to the 'any' filter value", () => {
  const d = makeTasksPrStatusFilter({ prStatus: "passing" });
  d.renderTtPrStatusFilter();
  d.statusDropdownToggleMenu("tasks-pr", d.fakeClick());
  d.statusDropdownSelectSingle("tasks-pr", null);
  assert.equal(d.taskTabFilters.prStatus, "any", "the shared component's null must round-trip to the filter's 'any', not literal null");
});

test("tasks: outside click (statusDropdownCloseAll) dismisses the open menu without changing the filter", () => {
  const d = makeTasksPrStatusFilter({ prStatus: "pending" });
  d.renderTtPrStatusFilter();
  d.statusDropdownToggleMenu("tasks-pr", d.fakeClick());
  d.statusDropdownCloseAll();
  assert.ok(!d.byIdMap.has(d.statusDropdownMenuId("tasks-pr")));
  assert.equal(d.taskTabFilters.prStatus, "pending", "dismissing without a pick must leave the filter untouched");
});

test("tasks: Escape closes the open menu and returns focus to the trigger", () => {
  const d = makeTasksPrStatusFilter();
  d.renderTtPrStatusFilter();
  d.statusDropdownToggleMenu("tasks-pr", d.fakeClick());
  const menu = d.byIdMap.get(d.statusDropdownMenuId("tasks-pr"));
  d.statusDropdownMenuKeydown({ key: "Escape", currentTarget: menu });
  assert.ok(!d.byIdMap.has(d.statusDropdownMenuId("tasks-pr")));
  assert.equal(d.trigger._focused, true);
});

// ---------- Reviews sidebar ----------

test("reviews: PR-status dropdown defaults to 'any' (off), translated to selectedValue=null, mirroring the Tasks tab's", () => {
  const d = makeReviewsPrStatusFilter();
  const config = d.reviewPrStatusDropdownConfig();
  assert.equal(config.id, "reviews-pr");
  assert.equal(config.mode, "single");
  assert.equal(config.selectedValue, null);
  assert.deepEqual(config.options.map((o) => o.value).sort(), ["failing", "passing", "pending"]);
});

test("reviews: clicking the trigger opens the menu and it stays open across a same-tick re-render", () => {
  const d = makeReviewsPrStatusFilter();
  d.renderReviewPrStatusFilter();
  d.statusDropdownToggleMenu("reviews-pr", d.fakeClick());
  assert.ok(d.byIdMap.has(d.statusDropdownMenuId("reviews-pr")), "menu must be open after the trigger click");

  d.renderReviewPrStatusFilter();
  assert.ok(d.byIdMap.has(d.statusDropdownMenuId("reviews-pr")), "a same-tick re-render must not close the open menu");
  const trigger = d.byIdMap.get(d.statusDropdownTriggerId("reviews-pr"));
  assert.equal(trigger.getAttribute("aria-expanded"), "true");
});

test("reviews: picking a status closes the menu, updates reviewFilters.prStatus, and re-renders/syncs the hash", () => {
  const d = makeReviewsPrStatusFilter();
  d.renderReviewPrStatusFilter();
  d.statusDropdownToggleMenu("reviews-pr", d.fakeClick());
  d.statusDropdownSelectSingle("reviews-pr", "passing");
  assert.equal(d.reviewFilters.prStatus, "passing");
  assert.ok(!d.byIdMap.has(d.statusDropdownMenuId("reviews-pr")), "picking a status must close the menu");
  assert.equal(d.calls.renderReviews, 1);
  assert.equal(d.calls.syncHash, 1);
});

test("reviews: picking 'Any' translates the shared component's null back to the 'any' filter value", () => {
  const d = makeReviewsPrStatusFilter({ prStatus: "failing" });
  d.renderReviewPrStatusFilter();
  d.statusDropdownToggleMenu("reviews-pr", d.fakeClick());
  d.statusDropdownSelectSingle("reviews-pr", null);
  assert.equal(d.reviewFilters.prStatus, "any");
});

test("reviews: outside click dismisses the open menu without changing the filter", () => {
  const d = makeReviewsPrStatusFilter({ prStatus: "passing" });
  d.renderReviewPrStatusFilter();
  d.statusDropdownToggleMenu("reviews-pr", d.fakeClick());
  d.statusDropdownCloseAll();
  assert.ok(!d.byIdMap.has(d.statusDropdownMenuId("reviews-pr")));
  assert.equal(d.reviewFilters.prStatus, "passing");
});

test("reviews: Escape closes the open menu and returns focus to the trigger", () => {
  const d = makeReviewsPrStatusFilter();
  d.renderReviewPrStatusFilter();
  d.statusDropdownToggleMenu("reviews-pr", d.fakeClick());
  const menu = d.byIdMap.get(d.statusDropdownMenuId("reviews-pr"));
  d.statusDropdownMenuKeydown({ key: "Escape", currentTarget: menu });
  assert.ok(!d.byIdMap.has(d.statusDropdownMenuId("reviews-pr")));
  assert.equal(d.trigger._focused, true);
});
