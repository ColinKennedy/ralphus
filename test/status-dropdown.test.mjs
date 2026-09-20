// Coverage for RAL-475: the shared Status dropdown component used by
// Squads, Tasks, Reviews, Queue, and Worktree Retirement. See
// ./board-status-dropdown.mjs for how this is sliced out of the real board
// chunk (librarian/assets/board/06-status-dropdown.js).
//
// Run with `npm test` (node --test).

import test from "node:test";
import assert from "node:assert/strict";
import { makeStatusDropdown, fakeTriggerClick } from "./board-status-dropdown.mjs";

// ---------- alphabetical ordering ----------

test("statusDropdownSortedOptions sorts by displayed label, not the raw identifier", () => {
  const { statusDropdownSortedOptions } = makeStatusDropdown();
  const options = [
    { value: "zzz_first_raw", label: "Apple", color: "--done" },
    { value: "aaa_second_raw", label: "Banana", color: "--pending" },
  ];
  const sorted = statusDropdownSortedOptions(options);
  assert.deepEqual(sorted.map((o) => o.label), ["Apple", "Banana"]);
});

test("statusDropdownSortedOptions uses locale-aware comparison (localeCompare), not ordinal codepoint order", () => {
  const { statusDropdownSortedOptions } = makeStatusDropdown();
  // Ordinal/codepoint order would put the accented "Ábc" after "Zzz"
  // (uppercase Á is a higher codepoint than 'Z'); localeCompare treats it
  // as sorting near "Abc".
  const options = [
    { value: "z", label: "Zzz", color: "--done" },
    { value: "a", label: "Ábc", color: "--pending" },
  ];
  const sorted = statusDropdownSortedOptions(options);
  assert.deepEqual(sorted.map((o) => o.label), ["Ábc", "Zzz"]);
});

test("statusDropdownMultiRowsHtml renders rows in alphabetical-by-label order", () => {
  const { statusDropdownMultiRowsHtml, config } = makeStatusDropdown();
  const html = statusDropdownMultiRowsHtml(config);
  const appleIdx = html.indexOf("Apple");
  const bananaIdx = html.indexOf("Banana");
  assert.ok(appleIdx >= 0 && bananaIdx >= 0);
  assert.ok(appleIdx < bananaIdx, "Apple (label-first) must render before Banana even though its raw value sorts last");
});

test("statusDropdownLabel converts a raw snake_case identifier into a human label", () => {
  const { statusDropdownLabel } = makeStatusDropdown();
  assert.equal(statusDropdownLabel("merge_stopped"), "Merge Stopped");
  assert.equal(statusDropdownLabel("done"), "Done");
});

// ---------- per-view status sets ----------

test("each dropdown id keeps its own config/selection in the shared registry, independent of other views", () => {
  const squads = makeStatusDropdown({ id: "squads", selected: new Set(["done"]), options: [{ value: "done", label: "Done", color: "--done" }] });
  const reviews = makeStatusDropdown({ id: "reviews", selected: new Set(["collecting"]), options: [{ value: "collecting", label: "Collecting", color: "--muted" }] });
  squads.renderStatusDropdown(squads.containerId, squads.config);
  reviews.renderStatusDropdown(reviews.containerId, reviews.config);
  assert.deepEqual([...squads.config.selected], ["done"]);
  assert.deepEqual([...reviews.config.selected], ["collecting"]);
  assert.notEqual(squads.config.options[0].value, reviews.config.options[0].value);
});

test("review lifecycle values are not mixed into a squad/task-shaped config", () => {
  const { config } = makeStatusDropdown({
    id: "squads",
    options: [{ value: "done", label: "Done", color: "--done" }, { value: "failed", label: "Failed", color: "--failed" }],
  });
  const values = config.options.map((o) => o.value);
  for (const guardianOnly of ["collecting", "merge_stopped", "merging", "in_review", "deployed"]) {
    assert.ok(!values.includes(guardianOnly), `${guardianOnly} must not appear in a squad/task Status dropdown's options`);
  }
});

// ---------- toggling ----------

test("statusDropdownToggleOption fires onToggle, mutates the selected Set, and keeps the menu open", () => {
  const d = makeStatusDropdown();
  d.renderStatusDropdown(d.containerId, d.config);
  d.statusDropdownToggleMenu(d.config.id, fakeTriggerClick(d));
  assert.ok(d.byIdMap.has(d.statusDropdownMenuId(d.config.id)), "menu should be open after toggling it");

  d.statusDropdownToggleOption(d.config.id, "zzz_first_raw", false);
  assert.deepEqual(d.calls.onToggle, [["zzz_first_raw", false]]);
  assert.deepEqual([...d.selected], ["aaa_second_raw"]);
  assert.ok(d.byIdMap.has(d.statusDropdownMenuId(d.config.id)), "toggling a status must not close the menu");
});

test("statusDropdownToggleOption refreshes the open menu's checked state in place", () => {
  const d = makeStatusDropdown({ selected: new Set(["aaa_second_raw"]) });
  d.renderStatusDropdown(d.containerId, d.config);
  d.statusDropdownToggleMenu(d.config.id, fakeTriggerClick(d));
  d.statusDropdownToggleOption(d.config.id, "zzz_first_raw", true);
  const menu = d.byIdMap.get(d.statusDropdownMenuId(d.config.id));
  assert.match(menu.innerHTML, /onchange="statusDropdownToggleOption\('test','zzz_first_raw',this\.checked\)"[^>]*checked|checked[^>]*onchange="statusDropdownToggleOption\('test','zzz_first_raw'/);
});

test("statusDropdownToggleOption refreshes the trigger's selected-count badge live", () => {
  const d = makeStatusDropdown({ selected: new Set(["zzz_first_raw", "aaa_second_raw"]) });
  d.renderStatusDropdown(d.containerId, d.config);
  // The fake document doesn't parse `innerHTML` strings into elements, so
  // register a stand-in trigger element the way a real browser would have
  // after parsing the container's rendered markup.
  const trigger = d.makeElement();
  trigger.id = d.statusDropdownTriggerId(d.config.id);
  trigger.innerHTML = d.statusDropdownTriggerLabelHtml(d.config);
  d.byIdMap.set(trigger.id, trigger);
  assert.doesNotMatch(trigger.innerHTML, /chip/, "full selection shows no count badge");
  d.statusDropdownToggleOption(d.config.id, "zzz_first_raw", false);
  assert.match(trigger.innerHTML, /1\/2/, "trigger badge must update live while the menu is open");
});

// ---------- All / None ----------

test("statusDropdownSelectAll and statusDropdownSelectNone fire their callbacks and keep the menu open", () => {
  const d = makeStatusDropdown();
  d.renderStatusDropdown(d.containerId, d.config);
  d.statusDropdownToggleMenu(d.config.id, fakeTriggerClick(d));

  d.statusDropdownSelectNone(d.config.id);
  assert.equal(d.calls.onNone, 1);
  assert.ok(d.byIdMap.has(d.statusDropdownMenuId(d.config.id)), "None must not close the menu");

  d.statusDropdownSelectAll(d.config.id);
  assert.equal(d.calls.onAll, 1);
  assert.ok(d.byIdMap.has(d.statusDropdownMenuId(d.config.id)), "All must not close the menu");
});

test("multi-mode menu renders All/None actions in the header", () => {
  const { statusDropdownMultiRowsHtml, config } = makeStatusDropdown();
  const html = statusDropdownMultiRowsHtml(config);
  assert.match(html, /onclick="statusDropdownSelectAll\('test'\)"/);
  assert.match(html, /onclick="statusDropdownSelectNone\('test'\)"/);
});

// ---------- outside-click / Escape dismissal ----------

test("statusDropdownCloseAll removes every open menu and resets every trigger's aria-expanded", () => {
  const d = makeStatusDropdown();
  d.renderStatusDropdown(d.containerId, d.config);
  d.statusDropdownToggleMenu(d.config.id, fakeTriggerClick(d));
  assert.ok(d.byIdMap.has(d.statusDropdownMenuId(d.config.id)));

  d.statusDropdownCloseAll();
  assert.ok(!d.byIdMap.has(d.statusDropdownMenuId(d.config.id)), "outside click must remove the open menu");
  const trigger = d.byIdMap.get(d.statusDropdownTriggerId(d.config.id));
  assert.equal(trigger.getAttribute("aria-expanded"), "false");
});

test("statusDropdownToggleMenu closes the menu when its own trigger is clicked again", () => {
  const d = makeStatusDropdown();
  d.renderStatusDropdown(d.containerId, d.config);
  const event = fakeTriggerClick(d);
  d.statusDropdownToggleMenu(d.config.id, event);
  assert.ok(d.byIdMap.has(d.statusDropdownMenuId(d.config.id)));
  d.statusDropdownToggleMenu(d.config.id, event);
  assert.ok(!d.byIdMap.has(d.statusDropdownMenuId(d.config.id)), "clicking the trigger again must close the menu");
});

test("statusDropdownMenuKeydown closes the menu on Escape and returns focus to the trigger", () => {
  const d = makeStatusDropdown();
  d.renderStatusDropdown(d.containerId, d.config);
  d.statusDropdownToggleMenu(d.config.id, fakeTriggerClick(d));
  const menu = d.byIdMap.get(d.statusDropdownMenuId(d.config.id));
  d.statusDropdownMenuKeydown({ key: "Escape", currentTarget: menu });
  assert.ok(!d.byIdMap.has(d.statusDropdownMenuId(d.config.id)), "Escape must close the menu");
  const trigger = d.byIdMap.get(d.statusDropdownTriggerId(d.config.id));
  assert.equal(trigger._focused, true, "Escape must return focus to the trigger");
});

test("statusDropdownMenuKeydown ignores non-Escape keys", () => {
  const d = makeStatusDropdown();
  d.renderStatusDropdown(d.containerId, d.config);
  d.statusDropdownToggleMenu(d.config.id, fakeTriggerClick(d));
  const menu = d.byIdMap.get(d.statusDropdownMenuId(d.config.id));
  d.statusDropdownMenuKeydown({ key: "Tab", currentTarget: menu });
  assert.ok(d.byIdMap.has(d.statusDropdownMenuId(d.config.id)), "Tab must not close the menu");
});

// ---------- re-render / aria-expanded preservation ----------

test("renderStatusDropdown preserves aria-expanded=true across a full re-render while the menu stays open", () => {
  const d = makeStatusDropdown();
  d.renderStatusDropdown(d.containerId, d.config);
  d.statusDropdownToggleMenu(d.config.id, fakeTriggerClick(d));
  let trigger = d.byIdMap.get(d.statusDropdownTriggerId(d.config.id));
  assert.equal(trigger.getAttribute("aria-expanded"), "true");

  // Simulate a view's All/None handler reassigning the Set and re-rendering
  // (renderStatusDropdown replaces the trigger's innerHTML wholesale).
  d.renderStatusDropdown(d.containerId, d.config);
  trigger = d.byIdMap.get(d.statusDropdownTriggerId(d.config.id));
  assert.equal(trigger.getAttribute("aria-expanded"), "true", "a re-render while the menu is open must not reset aria-expanded");
});

test("renderStatusDropdown leaves aria-expanded=false when the menu isn't open", () => {
  const d = makeStatusDropdown();
  d.renderStatusDropdown(d.containerId, d.config);
  assert.match(d.container.innerHTML, /aria-expanded="false"/, "the freshly rendered trigger markup must start closed");
});

// ---------- accessible labels / keyboard shape ----------

test("the trigger is a real <button> with aria-haspopup/aria-controls wired to the menu id", () => {
  const { statusDropdownTriggerHtml, config, statusDropdownMenuId } = makeStatusDropdown();
  const html = statusDropdownTriggerHtml(config);
  assert.match(html, /<button /);
  assert.match(html, /aria-haspopup="true"/);
  assert.match(html, new RegExp(`aria-controls="${statusDropdownMenuId(config.id)}"`));
});

test("multi-mode rows use native checkboxes exposing checked state", () => {
  const { statusDropdownMultiRowsHtml, config } = makeStatusDropdown({ selected: new Set(["zzz_first_raw"]) });
  const html = statusDropdownMultiRowsHtml(config);
  assert.match(html, /<input type="checkbox" checked[^>]*onchange="statusDropdownToggleOption\('test','zzz_first_raw'/);
  assert.match(html, /<input type="checkbox" (?!checked)[^>]*onchange="statusDropdownToggleOption\('test','aaa_second_raw'/);
});

test("row labels stop click propagation so a checkbox click doesn't bubble to the outside-click handler", () => {
  const { statusDropdownMultiRowsHtml, config } = makeStatusDropdown();
  const html = statusDropdownMultiRowsHtml(config);
  assert.match(html, /<label[^>]*onclick="event\.stopPropagation\(\)"/);
});

// ---------- single-select mode (RAL-474 forward-compat) ----------

test("single mode renders one radio row per option plus an 'any' row, alphabetical by label", () => {
  const d = makeStatusDropdown({ mode: "single", selectedValue: null });
  const html = d.statusDropdownSingleRowsHtml(d.config);
  const anyIdx = html.indexOf("Any");
  const appleIdx = html.indexOf("Apple");
  const bananaIdx = html.indexOf("Banana");
  assert.ok(anyIdx >= 0 && appleIdx >= 0 && bananaIdx >= 0);
  assert.ok(appleIdx < bananaIdx);
});

test("statusDropdownSelectSingle fires onSelect and closes the menu", () => {
  const d = makeStatusDropdown({ mode: "single", selectedValue: null });
  d.renderStatusDropdown(d.containerId, d.config);
  d.statusDropdownToggleMenu(d.config.id, fakeTriggerClick(d));
  d.statusDropdownSelectSingle(d.config.id, "aaa_second_raw");
  assert.deepEqual(d.calls.onSelect, ["aaa_second_raw"]);
  assert.ok(!d.byIdMap.has(d.statusDropdownMenuId(d.config.id)), "picking a value in single mode must close the menu");
});
