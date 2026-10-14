// RAL-655: the review menu's "Stop actioning" enablement and the per-branch
// "auto-fix paused" badge, sliced out of the real board source.
import test from "node:test";
import assert from "node:assert/strict";
import { boardSource } from "./board-exhausted-badges.mjs";

const BEGIN = "// RALPHUS-STOP-ACTIONING:BEGIN";
const END = "// RALPHUS-STOP-ACTIONING:END";
const region = boardSource.slice(boardSource.indexOf(BEGIN) + BEGIN.length, boardSource.indexOf(END));
// eslint-disable-next-line no-new-func -- evaluating the shipped source is the point.
const { reviewIsActioning, autoFixPausedBadge } = new Function(
  "esc",
  `${region}\nreturn { reviewIsActioning, autoFixPausedBadge };`,
)((s) => String(s ?? ""));

test("reviewIsActioning is true only for active branch statuses", () => {
  for (const s of ["in_progress", "proof_pending", "actioning"]) {
    assert.equal(reviewIsActioning({ branches: [{ merge_status: "done" }, { merge_status: s }] }), true, s);
  }
  assert.equal(reviewIsActioning({ branches: [{ merge_status: "done" }, { merge_status: "failed" }] }), false);
  assert.equal(reviewIsActioning({ branches: [{ merge_status: "actioning", enabled: false }] }), false);
  assert.equal(reviewIsActioning({}), false);
  assert.equal(reviewIsActioning(null), false);
});

test("autoFixPausedBadge renders only when the latch is set, with a tooltip", () => {
  assert.equal(autoFixPausedBadge({ id: "b" }), "");
  const html = autoFixPausedBadge({ id: "b", auto_fix_paused_at_ms: 5 });
  assert.match(html, /auto-fix paused/);
  assert.match(html, /data-tip=/);
});

test("the menu item and handler are wired", () => {
  assert.match(boardSource, /CLICK_HANDLERS\.stopActioning/);
  assert.match(boardSource, /data-click="stopActioning"/);
});
