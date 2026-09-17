// Coverage for RAL-430 "Preserve scroll positions in text widgets":
//
// - `preserveUserState` restores a nested scrollable descendant's scroll
//   offset (the live terminal / system-prompt peek boxes) across a render
//   that tears down and rebuilds the DOM under it -- the exact thing
//   `el.innerHTML = ...` does on every periodic refresh;
// - it also restores the *container's* own scroll offset, not just a
//   descendant's;
// - it restores a focused input's value/selection and refocuses it, so an
//   in-progress edit survives the same refresh (RAL-7, unchanged by this
//   ticket -- pinned here so a future refactor of the shared helper can't
//   silently drop it while chasing the scroll fix);
// - a scrollTop of 0 is not tracked (nothing to restore) and a missing `el`
//   just runs the render with no bookkeeping;
// - `rerenderOwningPane` -- the RAL-430 helper now used by every peek/menu/
//   tab-toggle call site that used to call `renderDetails()`/
//   `renderReviewDetail()` directly -- routes both branches through the real
//   `preserveUserState`, and only touches whichever pane actually owns the
//   current selection.
//
// Run with `npm test` (node --test). See ./board-preserve-scroll.mjs for how
// the regions are loaded out of the real board chunks and the fake DOM they
// run against.

import test from "node:test";
import assert from "node:assert/strict";
import { makePreserveUserState, makeRerenderOwningPane, FakeNode, FakeInput } from "./board-preserve-scroll.mjs";

/** Replaces `el`'s children with a fresh subtree (mimics an `innerHTML` swap: old nodes are gone, new ones share the old ids but start at their defaults). */
function rebuild(document, el, newChildren) {
  el.children = [];
  for (const c of newChildren) el.appendChild(c);
  document.register(el);
}

test("preserveUserState restores a nested descendant's scrollTop across a rebuild", () => {
  const { preserveUserState, document } = makePreserveUserState();
  const details = new FakeNode("details");
  const peekPre = new FakeNode("peek-pre-cell|sq-1|0|0");
  peekPre.scrollTop = 240;
  rebuild(document, details, [peekPre]);

  preserveUserState(details, () => {
    // simulate the periodic refresh rebuilding the pane from scratch
    rebuild(document, details, [new FakeNode("peek-pre-cell|sq-1|0|0")]);
  });

  assert.equal(document.getElementById("peek-pre-cell|sq-1|0|0").scrollTop, 240);
});

test("preserveUserState restores the container's own scrollTop, not just a descendant's", () => {
  const { preserveUserState, document } = makePreserveUserState();
  const reviewDetail = new FakeNode("review-detail");
  reviewDetail.scrollTop = 88;
  document.register(reviewDetail);

  preserveUserState(reviewDetail, () => {
    reviewDetail.scrollTop = 0; // a shorter re-render can reflow the container back to 0
  });

  assert.equal(reviewDetail.scrollTop, 88);
});

test("preserveUserState leaves an untouched (0) scrollTop alone -- nothing to restore", () => {
  const { preserveUserState, document } = makePreserveUserState();
  const details = new FakeNode("details");
  const box = new FakeNode("system-prompt-box-0-0");
  rebuild(document, details, [box]);

  let sawZero = false;
  preserveUserState(details, () => {
    const fresh = new FakeNode("system-prompt-box-0-0");
    fresh.scrollTop = 5; // rebuilt node happens to start non-zero
    rebuild(document, details, [fresh]);
    sawZero = document.getElementById("system-prompt-box-0-0").scrollTop === 5;
  });

  assert.ok(sawZero, "render ran");
  // untracked (was 0 before render) -- preserveUserState never touches it
  assert.equal(document.getElementById("system-prompt-box-0-0").scrollTop, 5);
});

test("preserveUserState restores a focused input's value, selection, and focus", () => {
  const { preserveUserState, document } = makePreserveUserState();
  const details = new FakeNode("details");
  const input = new FakeInput("cell-note", "hello world");
  input.selectionStart = 2;
  input.selectionEnd = 5;
  rebuild(document, details, [input]);
  document.setActive(input);

  preserveUserState(details, () => {
    rebuild(document, details, [new FakeInput("cell-note", "")]);
  });

  const restored = document.getElementById("cell-note");
  assert.equal(restored.value, "hello world");
  assert.equal(restored.selectionStart, 2);
  assert.equal(restored.selectionEnd, 5);
  assert.equal(document.activeElement, restored, "the new node is refocused");
});

test("preserveUserState with a null el just runs the render", () => {
  const { preserveUserState } = makePreserveUserState();
  let ran = false;
  assert.doesNotThrow(() => preserveUserState(null, () => { ran = true; }));
  assert.ok(ran);
});

test("rerenderOwningPane preserves the details pane's scroll when a cell/task/proof is selected", () => {
  let renderCalls = 0;
  const h = makeRerenderOwningPane({
    sel: { kind: "cell", taskIdx: 0, cellIdx: 0 },
    selectedGuardian: null,
    renderDetails: () => {
      renderCalls++;
      rebuild(h.document, details, [new FakeNode("peek-pre-cell|sq-1|0|0")]);
    },
    renderReviewDetail: () => { throw new Error("must not render the review pane when nothing is selected"); },
  });
  const details = new FakeNode("details");
  const peekPre = new FakeNode("peek-pre-cell|sq-1|0|0");
  peekPre.scrollTop = 400;
  rebuild(h.document, details, [peekPre]);

  h.rerenderOwningPane();

  assert.equal(renderCalls, 1);
  assert.equal(h.document.getElementById("peek-pre-cell|sq-1|0|0").scrollTop, 400);
});

test("rerenderOwningPane preserves the review-detail pane's scroll when a guardian is selected", () => {
  let renderCalls = 0;
  const h = makeRerenderOwningPane({
    sel: { kind: null },
    selectedGuardian: "g-1",
    renderDetails: () => { throw new Error("must not render the details pane when nothing is selected"); },
    renderReviewDetail: () => {
      renderCalls++;
      rebuild(h.document, reviewDetail, [new FakeNode("peek-pre-guardian|g-1|branch-1")]);
    },
  });
  const reviewDetail = new FakeNode("review-detail");
  const peekPre = new FakeNode("peek-pre-guardian|g-1|branch-1");
  peekPre.scrollTop = 150;
  rebuild(h.document, reviewDetail, [peekPre]);

  h.rerenderOwningPane();

  assert.equal(renderCalls, 1);
  assert.equal(h.document.getElementById("peek-pre-guardian|g-1|branch-1").scrollTop, 150);
});

test("rerenderOwningPane is a no-op for either pane when neither a selection nor a guardian is active", () => {
  const h = makeRerenderOwningPane({
    sel: { kind: null },
    selectedGuardian: null,
    renderDetails: () => { throw new Error("must not render"); },
    renderReviewDetail: () => { throw new Error("must not render"); },
  });
  assert.doesNotThrow(() => h.rerenderOwningPane());
});
