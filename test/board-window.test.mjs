// STRESS-004: the board loads active squads plus a window of recent finished
// ones, with older pages and by-id pins merged in.
import { test } from "node:test";
import assert from "node:assert/strict";

import { makeWindow } from "./board-window.mjs";

const sq = (n, extra = {}) => ({ id: `squad-${String(n).padStart(12, "0")}`, ...extra });

test("windowed URLs ask for the window, and a cursor for later pages", () => {
  const w = makeWindow();
  assert.equal(w.boardWindowUrl("/api/tasks"), `/api/tasks?window=${w.BOARD_WINDOW}`);
  assert.equal(
    w.boardWindowUrl("/api/task-index", "123:squad-000000000007"),
    `/api/task-index?window=${w.BOARD_WINDOW}&before=123%3Asquad-000000000007`,
  );
});

test("merging keeps the first page's copy of a squad and orders newest first", () => {
  const w = makeWindow();
  const merged = w.mergeWindowedSquads(
    [sq(9, { state: "running" }), sq(5)],
    [sq(3), sq(9, { state: "done" })],
    [sq(7)],
  );
  assert.deepEqual(merged.map((s) => s.id), [sq(9).id, sq(7).id, sq(5).id, sq(3).id]);
  assert.equal(merged[0].state, "running", "the first page is the freshest copy");
});

test("each shape's cursor follows the first page until that shape loads an older page", () => {
  const w = makeWindow();
  w.noteBoardWindow({ total: 900, loaded: 210, has_more: true, next_before: "c1" }, "full");
  assert.equal(w.boardWindow.cursor.full, "c1");
  assert.equal(w.boardWindow.hasMore.full, true);

  // "Load older" took over: the first page's cursor must not rewind it.
  w.boardWindow.older.full.push(sq(1));
  w.boardWindow.cursor.full = "c2";
  w.noteBoardWindow({ total: 901, loaded: 210, has_more: true, next_before: "c1" }, "full");
  assert.equal(w.boardWindow.cursor.full, "c2");
  assert.equal(w.boardWindow.meta.total, 901, "the total still updates");

  // An unwindowed (older daemon) response carries no window: nothing changes.
  w.noteBoardWindow(undefined, "index");
  assert.equal(w.boardWindow.cursor.index, null);
});

test("the list includes older pages and pinned squads", () => {
  const w = makeWindow();
  w.boardWindow.older.full.push(sq(2));
  w.boardWindow.pinned.set(sq(1).id, sq(1));
  const list = w.windowedSquads([sq(5)], "full");
  assert.deepEqual(list.map((s) => s.id), [sq(5).id, sq(2).id, sq(1).id]);
  // Older pages are per shape.
  assert.deepEqual(w.windowedSquads([sq(5)], "index").map((s) => s.id), [sq(5).id, sq(1).id]);
});

test("the footer appears only while older history is not loaded, and says how much is shown", () => {
  const w = makeWindow();
  assert.equal(w.boardWindowFooter(10, "full"), "");
  w.noteBoardWindow({ total: 900, loaded: 210, has_more: true, next_before: "c1" }, "full");
  const footer = w.boardWindowFooter(210, "full");
  assert.match(footer, /Showing 210 of 900 squads/);
  assert.match(footer, /data-click="loadOlderSquads"/);
  assert.match(footer, /data-tip=/, "every new control carries a tooltip");
  w.boardWindow.hasMore.full = false;
  assert.equal(w.boardWindowFooter(900, "full"), "");
});
