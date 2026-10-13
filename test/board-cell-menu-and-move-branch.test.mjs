// Coverage for two RAL-641 board changes:
//
// - a cell's actions live in a ⋯ menu at the top of its details pane, with
//   "Show ways to fix this block…" first whenever the cell is holding up other work;
// - a review branch has one "Move or extract branch…" action whose popup
//   offers a new review alongside every open review, replacing the separate
//   "Extract to a new review" and "Move to another review…" items;
// - a review branch row carries no inline "Ignore and continue" button (on a
//   branch it would do nothing that Disable does not).
//
// Run with `npm test`. Same slice-the-real-source approach as
// ./board-forge-check.test.mjs: the RALPHUS-CELL-MENU-ACTIONS and
// RALPHUS-MOVE-BRANCH-TARGETS blocks are pure and need nothing from the page.

import test from "node:test";
import assert from "node:assert/strict";
import { boardScript } from "./board-source.mjs";

const html = boardScript();

/**
 * Evaluates the pure function `name` defined between `// <marker>:BEGIN`
 * and `// <marker>:END` in the board source.
 */
function slice(marker, name) {
  const begin = `// ${marker}:BEGIN`;
  const end = `// ${marker}:END`;
  const from = html.indexOf(begin);
  const to = html.indexOf(end);
  if (from === -1 || to === -1 || to < from) {
    throw new Error(
      `could not find the ${begin} / ${end} markers in the board chunks. ` +
        "If this logic moved, move the markers with it -- these tests are its only coverage.",
    );
  }
  return new Function(`${html.slice(from + begin.length, to)}\nreturn ${name};`)();
}

const cellMenuActions = slice("RALPHUS-CELL-MENU-ACTIONS", "cellMenuActions");
const moveBranchTargets = slice("RALPHUS-MOVE-BRANCH-TARGETS", "moveBranchTargets");
const moveBranchPickerItems = slice("RALPHUS-MOVE-BRANCH-TARGETS", "moveBranchPickerItems");

const blocking = { failure_count: 2, threshold: 2, blocked_cells: 1, blocked_branches: 1, branch: ["g-1", "b"] };
const keys = (actions) => actions.map((a) => a.key);

test("a blocking cell's menu leads with Show ways to fix this block…", () => {
  const actions = cellMenuActions("failed", blocking, true);
  assert.equal(actions[0].key, "unblock");
  assert.match(actions[0].label, /Show ways to fix this block…/);
  assert.deepEqual(keys(actions), ["unblock", "edit", "restart", "ignore", "copy", "copy-branch"]);
});

test("a cell that is not blocking has no fix-the-block item", () => {
  assert.ok(!keys(cellMenuActions("failed", undefined, true)).includes("unblock"));
  assert.ok(!keys(cellMenuActions("done", undefined, true)).includes("unblock"));
});

test("ignore is offered only while the cell can still be skipped", () => {
  for (const state of ["failed", "pending", "queued"]) {
    assert.ok(keys(cellMenuActions(state, undefined, true)).includes("ignore"), state);
  }
  for (const state of ["done", "running", "ignored", "cancelled"]) {
    assert.ok(!keys(cellMenuActions(state, undefined, true)).includes("ignore"), state);
  }
});

test("copy from its own work needs a project", () => {
  assert.ok(keys(cellMenuActions("failed", blocking, true)).includes("copy-branch"));
  assert.ok(!keys(cellMenuActions("failed", blocking, false)).includes("copy-branch"));
});

test("every cell menu item has a label and a tooltip", () => {
  for (const action of cellMenuActions("failed", blocking, true)) {
    assert.ok(action.label.length > 0, action.key);
    assert.ok(action.tip.length > 0, action.key);
  }
});

test("the ⋯ menu sits in the cell details header and its handlers are wired", () => {
  assert.match(html, /<span class="k">◉ cell<\/span>[^\n]*data-click="openCellMenu"/);
  assert.match(html, /CLICK_HANDLERS\.openCellMenu = /);
  assert.match(html, /CLICK_HANDLERS\.cellMenuPick = /);
});

test("move targets are the other reviews that can still take work", () => {
  const reviews = [
    { id: "g-1", name: "here", status: "collecting" },
    { id: "g-2", name: "open", status: "in_review" },
    { id: "g-3", name: "merged", status: "merged" },
    { id: "g-4", name: "approved", status: "approved" },
    { id: "g-5", name: "cancelled", status: "cancelled" },
    { id: "g-6", name: "deployed", status: "deployed" },
    { id: "g-7", name: "also open", status: "collecting" },
  ];
  assert.deepEqual(moveBranchTargets(reviews, "g-1").map((r) => r.id), ["g-2", "g-7"]);
});

test("a review branch has one Move or extract action instead of two", () => {
  assert.match(html, /data-click="openMoveOrExtractBranch"[^\n]*⇄ Move or extract branch…/);
  assert.doesNotMatch(html, /Extract to a new review<\/div>/);
  assert.doesNotMatch(html, /Move to another review…<\/div>/);
  assert.doesNotMatch(html, /openMoveBranchMenu/);
});

const reviews = [
  { id: "g-1", name: "here", status: "collecting" },
  { id: "g-2", name: "frontend", status: "in_review" },
  { id: "g-3", name: "backend", status: "collecting" },
  { id: "g-4", name: "old", status: "merged" },
];

test("the picker lists New review… first, then every open review", () => {
  const items = moveBranchPickerItems(reviews, "g-1", "");
  assert.deepEqual(items.map((i) => i.id), ["__new__", "g-2", "g-3"]);
  assert.equal(items[0].title, "New review…");
  assert.match(items[0].detail, /same base branch and project/);
  assert.match(items[1].detail, /Existing review · in_review/);
});

test("searching narrows the reviews but always keeps New review… on top", () => {
  assert.deepEqual(moveBranchPickerItems(reviews, "g-1", "front").map((i) => i.id), ["__new__", "g-2"]);
  assert.deepEqual(moveBranchPickerItems(reviews, "g-1", "G-3").map((i) => i.id), ["__new__", "g-3"]);
  assert.deepEqual(moveBranchPickerItems(reviews, "g-1", "nothing like it").map((i) => i.id), ["__new__"]);
});

test("the picker uses the Go To Task list styling and keys", () => {
  const start = html.indexOf("function openMoveOrExtractBranch(");
  const end = html.indexOf("function renderMoveBranchPicker(", start);
  const body = html.slice(start, end);
  assert.match(body, /class="modal goto-search-modal"/);
  assert.match(body, /class="goto-search-input"/);
  assert.match(body, /class="goto-search-list"/);
  assert.match(html, /class="goto-search-row\$\{index === moveBranchPicker\.selected \? " selected" : ""\}"/);
  assert.match(html, /function moveBranchPickerKeydown\(event\)/);
});

test("confirming sends a new review to extract and an existing one to move", () => {
  const start = html.indexOf("async function confirmMoveOrExtractBranch(");
  const end = html.indexOf("async function moveBranchTo(", start);
  const body = html.slice(start, end);
  assert.match(body, /if \(target === "__new__"\) await extractBranch\(gid, branchId\);/);
  assert.match(body, /else await moveBranchTo\(gid, branchId, target\);/);
});

test("a review branch row has no inline Ignore and continue button", () => {
  assert.doesNotMatch(html, /branchIgnoreBtn/);
});
