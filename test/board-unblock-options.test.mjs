// Coverage for the "Ways to fix the block" popup of a blocking cell (RAL-641): which
// options it offers and what each says it will do.
//
// - the two ways out are ignore and extract; copy is only ever an add-on, so
//   it is never presented as an alternative to them;
// - ignoring says it also takes the review branch out of the review (and says
//   nothing about a branch when the cell has none);
// - there is no standalone "disable branch" option, because ignoring already
//   does that;
// - extract is offered only when the cell has a review branch, and says the
//   cell is left as it is;
// - copy-from-its-own-work is offered only for a cell in a project;
// - the button sits beside the state row and every handler it needs is wired.
//
// Run with `npm test`. Same slice-the-real-source approach as
// ./board-forge-check.test.mjs: the RALPHUS-UNBLOCK-OPTIONS:BEGIN/END block in
// librarian/assets/board/40-dialogs.js is pure and needs nothing from the page.

import test from "node:test";
import assert from "node:assert/strict";
import { boardScript } from "./board-source.mjs";

const BEGIN = "// RALPHUS-UNBLOCK-OPTIONS:BEGIN";
const END = "// RALPHUS-UNBLOCK-OPTIONS:END";

const html = boardScript();
const from = html.indexOf(BEGIN);
const to = html.indexOf(END);
if (from === -1 || to === -1 || to < from) {
  throw new Error(
    `unblock-options: could not find the ${BEGIN} / ${END} markers in the board chunks. ` +
      "If this logic moved, move the markers with it -- these tests are its only coverage.",
  );
}
const unblockOptions = new Function(`${html.slice(from + BEGIN.length, to)}\nreturn unblockOptions;`)();

const blocking = (branch = ["guardian-1", "live-flaky"]) => ({
  failure_count: 2,
  threshold: 2,
  blocked_cells: 1,
  blocked_branches: 1,
  branch,
});

const keys = (options) => options.map((o) => o.key);

test("a cell with a branch and a project offers two ways out and two add-ons", () => {
  const options = unblockOptions(blocking(), true);
  assert.deepEqual(keys(options), ["ignore", "extract", "copy", "copy-branch"]);
  assert.deepEqual(
    options.filter((o) => !o.addOn).map((o) => o.key),
    ["ignore", "extract"],
  );
  assert.deepEqual(
    options.filter((o) => o.addOn).map((o) => o.key),
    ["copy", "copy-branch"],
  );
});

test("ignore says it also takes the review branch out of the review", () => {
  const ignore = unblockOptions(blocking(), true).find((o) => o.key === "ignore");
  assert.match(ignore.effect, /re-runs the cells that failed only because of it/);
  assert.match(ignore.effect, /takes its review branch out of the review/);
});

test("ignore does not mention a branch when the cell has none", () => {
  const options = unblockOptions(blocking(null), true);
  const ignore = options.find((o) => o.key === "ignore");
  assert.doesNotMatch(ignore.effect, /branch/);
  assert.ok(!keys(options).includes("extract"));
});

test("disabling the branch is not offered as its own option", () => {
  for (const option of unblockOptions(blocking(), true)) {
    assert.doesNotMatch(option.title, /disable/i);
    assert.notEqual(option.key, "disable");
  }
});

test("extract says the cell is left as it is for a retry", () => {
  const extract = unblockOptions(blocking(), true).find((o) => o.key === "extract");
  assert.match(extract.effect, /review of its own/);
  assert.match(extract.effect, /cell is left as it is/);
  assert.match(extract.effect, /same cell can be retried/);
});

test("copy says it starts from the latest main code and leaves the original alone", () => {
  const copy = unblockOptions(blocking(), true).find((o) => o.key === "copy");
  assert.match(copy.effect, /one-task squad/);
  assert.match(copy.effect, /latest main code/);
  assert.match(copy.effect, /original cell is not changed/);
});

test("copy from the cell's own work is offered only for a cell in a project", () => {
  assert.ok(keys(unblockOptions(blocking(), true)).includes("copy-branch"));
  const without = unblockOptions(blocking(), false);
  assert.ok(!keys(without).includes("copy-branch"));
  assert.deepEqual(keys(without), ["ignore", "extract", "copy"]);
  const fromWork = unblockOptions(blocking(), true).find((o) => o.key === "copy-branch");
  assert.match(fromWork.effect, /cell's own saved work/);
});

test("every option has a title and an effect", () => {
  for (const option of unblockOptions(blocking(), true)) {
    assert.ok(option.title.length > 0);
    assert.ok(option.effect.length > 0);
  }
});

test("the popup is reached from the cell's ⋯ menu, not a button beside the state", () => {
  assert.doesNotMatch(html, /unblockButton\(/);
  assert.match(html, /if \(action === "unblock"\) openUnblockCell\(squadId, ti, si\);/);
  assert.match(html, /CLICK_HANDLERS\.unblockPick = /);
});

test("extract looks the branch up from the daemon, not the lean review list", () => {
  const start = html.indexOf("async function unblockPick(");
  const end = html.indexOf("async function copyCell(", start);
  assert.ok(start !== -1 && end > start, "could not find unblockPick");
  const body = html.slice(start, end);
  assert.match(body, /fetch\(`\/api\/guardians\/\$\{encodeURIComponent\(guardianId\)\}`\)/);
  assert.doesNotMatch(body, /guardians\.find/);
});

test("picking an option does not ask a second time", () => {
  assert.match(html, /async function ignoreCell\(squadId, ti, si, confirmed\)/);
  assert.match(html, /ignoreCell\(squadId, ti, si, true\)/);
  // extractBranch never prompts; every caller (this popup, Move or extract) is the confirmation.
  assert.match(html, /async function extractBranch\(gid, branchId\) \{\s*await guardianAction/);
  assert.match(html, /extractBranch\(guardianId, entry\.id\)/);
});

test("the popup heads the ways out with To unblock… and the copies with Optionally…", () => {
  const start = html.indexOf("function openUnblockCell(");
  const end = html.indexOf("async function unblockPick(", start);
  const body = html.slice(start, end);
  const unblockAt = body.indexOf(">To unblock…</h3>");
  const mainAt = body.indexOf("${main}");
  const optionallyAt = body.indexOf(">Optionally…</h3>");
  const addOnsAt = body.indexOf("${addOns}");
  assert.ok(unblockAt !== -1 && optionallyAt !== -1, "both headers are present");
  assert.ok(unblockAt < mainAt && mainAt < optionallyAt && optionallyAt < addOnsAt, "each header sits right above its section");
});
