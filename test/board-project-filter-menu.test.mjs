// Coverage for RAL-439: the Squads sidebar's and Tasks toolbar's
// project-filter dropdowns each toggle a project on/off correctly and keep
// their own state independent. See ./board-project-filter-menu.mjs for how
// this is sliced out of the real board chunks.
//
// Run with `npm test` (node --test).

import test from "node:test";
import assert from "node:assert/strict";
import { makeSquadsProjectFilterMenu, makeTasksProjectFilterMenu } from "./board-project-filter-menu.mjs";

// ---------- Squads sidebar ----------

test("toggleProjectFilter (squads) adds and removes a project from the filter", () => {
  const { toggleProjectFilter, filters, calls } = makeSquadsProjectFilterMenu();
  toggleProjectFilter("acme", true);
  assert.deepEqual([...filters.project], ["acme"]);
  assert.equal(calls.renderSquads, 1);
  assert.equal(calls.syncHash, 1);
  toggleProjectFilter("acme", false);
  assert.deepEqual([...filters.project], []);
  assert.equal(calls.renderSquads, 2);
});

test("toggleProjectFilter (squads) updates an already-open menu's checkmarks in place rather than closing it", () => {
  const { toggleProjectFilter, byIdMap } = makeSquadsProjectFilterMenu();
  const menu = { innerHTML: "" };
  byIdMap.set("project-filter-menu", menu);
  toggleProjectFilter("acme", true);
  assert.ok(menu.innerHTML.includes('checked'));
  assert.ok(byIdMap.has("project-filter-menu"), "toggling must not remove the open menu");
});

test("projectFilterMenuRowsHtml (squads) stops row clicks from bubbling to the document-level close listener", () => {
  // RAL-439 regression guard: without stopPropagation, a click on the
  // checkbox/label bubbles to document's closeProjectFilterMenu and tears
  // the dropdown down out from under the very click meant to check a box.
  const { projectFilterMenuRowsHtml } = makeSquadsProjectFilterMenu();
  const rowsHtml = projectFilterMenuRowsHtml();
  assert.match(rowsHtml, /<label[^>]*onclick="event\.stopPropagation\(\)"/);
});

test("projectFilterMenuRowsHtml (squads) reflects current selection via the 'on'/'checked' state", () => {
  const { projectFilterMenuRowsHtml } = makeSquadsProjectFilterMenu({ filters: { project: new Set(["beta"]) } });
  const rowsHtml = projectFilterMenuRowsHtml();
  assert.match(rowsHtml, /ctx-check on"><label[^>]*><input type="checkbox" checked[^>]*onchange="toggleProjectFilter\('beta'/);
  assert.doesNotMatch(rowsHtml, /ctx-check on"><label[^>]*><input type="checkbox" checked[^>]*onchange="toggleProjectFilter\('acme'/);
});

test("clearProjectFilter (squads) empties the filter and closes the menu", () => {
  const { toggleProjectFilter, clearProjectFilter, filters, byIdMap } = makeSquadsProjectFilterMenu();
  toggleProjectFilter("acme", true);
  byIdMap.set("project-filter-menu", { innerHTML: "", remove: () => byIdMap.delete("project-filter-menu") });
  clearProjectFilter();
  assert.equal(filters.project.size, 0);
  assert.equal(byIdMap.has("project-filter-menu"), false);
});

// ---------- Tasks toolbar ----------

test("ttToggleProjectFilter (tasks) adds and removes a project from its own filter, independent of the squads filter", () => {
  const { ttToggleProjectFilter, taskTabFilters, calls } = makeTasksProjectFilterMenu();
  ttToggleProjectFilter("acme", true);
  assert.deepEqual([...taskTabFilters.project], ["acme"]);
  assert.equal(calls.renderTasksTab, 1);
  assert.equal(calls.ttScrollSelectionIntoView, 1);
  assert.equal(calls.syncHash, 1);
  ttToggleProjectFilter("acme", false);
  assert.deepEqual([...taskTabFilters.project], []);
});

test("ttToggleProjectFilter (tasks) updates an already-open menu's checkmarks in place rather than closing it", () => {
  const { ttToggleProjectFilter, byIdMap } = makeTasksProjectFilterMenu();
  const menu = { innerHTML: "" };
  byIdMap.set("tt-project-filter-menu", menu);
  ttToggleProjectFilter("acme", true);
  assert.ok(menu.innerHTML.includes("checked"));
  assert.ok(byIdMap.has("tt-project-filter-menu"), "toggling must not remove the open menu");
});

test("ttProjectFilterMenuRowsHtml (tasks) stops row clicks from bubbling to the document-level close listener", () => {
  // RAL-439 regression guard, Tasks-tab side of the same bug.
  const { ttProjectFilterMenuRowsHtml } = makeTasksProjectFilterMenu();
  const rowsHtml = ttProjectFilterMenuRowsHtml();
  assert.match(rowsHtml, /<label[^>]*onclick="event\.stopPropagation\(\)"/);
});

test("ttClearProjectFilter (tasks) empties the filter and closes the menu", () => {
  const { ttToggleProjectFilter, ttClearProjectFilter, taskTabFilters, byIdMap } = makeTasksProjectFilterMenu();
  ttToggleProjectFilter("acme", true);
  byIdMap.set("tt-project-filter-menu", { innerHTML: "", remove: () => byIdMap.delete("tt-project-filter-menu") });
  ttClearProjectFilter();
  assert.equal(taskTabFilters.project.size, 0);
  assert.equal(byIdMap.has("tt-project-filter-menu"), false);
});

test("Squads and Tasks project filters are independent Set instances", () => {
  const squads = makeSquadsProjectFilterMenu();
  const tasks = makeTasksProjectFilterMenu();
  squads.toggleProjectFilter("acme", true);
  assert.deepEqual([...squads.filters.project], ["acme"]);
  assert.equal(tasks.taskTabFilters.project.size, 0);
});
