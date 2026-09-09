// Coverage for RAL-382 "Open Task Review Badges Without Delay":
//
// - the Tasks table's compact review badge is clickable (opens the review the
//   badge displays — the attention-ranked pick) and its +N suffix is a separate
//   affordance that opens the Reviews tab's full list instead of silently
//   picking one of the other reviews;
// - gotoReview renders the Reviews tab synchronously (no lingering previous
//   selection) and flags a not-yet-loaded target with the loading placeholder;
// - rapid clicks on different review badges leave the last-clicked review
//   selected;
// - overlapping pollReviews calls that complete out of order can't overwrite
//   the fresher poll's data/render (latest-wins sequence guard).
//
// Run with `npm test` (node --test). See ./board-review-badge-nav.mjs for how
// the regions are loaded out of the real board chunks.

import test from "node:test";
import assert from "node:assert/strict";
import { boardSource, makeBadgeRenderer, makeGotoReview, makePollReviews, resolveJson } from "./board-review-badge-nav.mjs";

// ---------- badge lane markup (sliced real source) ----------

test("the compact review badge is clickable and targets the review it displays", () => {
  const { ttReviewPrBadgesHtml } = makeBadgeRenderer();
  const html = ttReviewPrBadgesHtml(
    { review: { id: "guardian-1", name: "solo review", status: "in_review", origin: "explicit", branches: [] }, count: 1 },
    null,
  );
  assert.match(html, /data-click="gotoReview"/);
  assert.match(html, /data-guardian-id="guardian-1"/);
  assert.match(html, /class="tt-badge review-badge"/);
  // single review: no +N affordance
  assert.doesNotMatch(html, /data-click="openReviewList"/);
});

test("a multi-review task keeps +N as a separate affordance for the full review list", () => {
  const { ttReviewPrBadgesHtml } = makeBadgeRenderer();
  const html = ttReviewPrBadgesHtml(
    { review: { id: "guardian-1", name: "best review", status: "in_review", origin: "explicit", branches: [] }, count: 3 },
    null,
  );
  // badge body opens the displayed review...
  assert.match(html, /data-click="gotoReview"/);
  assert.match(html, /data-guardian-id="guardian-1"/);
  // ...and +N is its own click target for the full list, carrying the count
  assert.match(html, /data-click="openReviewList"/);
  assert.match(html, />\+2<\/span>/);
  // the +N count no longer sits inside the gotoReview badge's own text
  const badge = html.slice(html.indexOf("review-badge"), html.indexOf("review-extra"));
  assert.doesNotMatch(badge, /\+2/);
});

test("the review badge tooltip explains both the jump and the +N affordance", () => {
  const { ttReviewPrBadgesHtml } = makeBadgeRenderer();
  const html = ttReviewPrBadgesHtml(
    { review: { id: "guardian-1", name: "some review", status: "in_review", origin: "explicit", branches: [] }, count: 2 },
    null,
  );
  assert.match(html, /Click to open this review on the Reviews tab\./);
  assert.match(html, /\+N opens the full list\./);
});

test("PR badge behavior is unchanged (inline onclick, opens the forge) and the approved-no-PR placeholder survives", () => {
  const { ttReviewPrBadgesHtml } = makeBadgeRenderer();
  const withPr = ttReviewPrBadgesHtml(
    { review: { id: "guardian-1", name: "r", status: "in_review", origin: "explicit", branches: [] }, count: 1 },
    { pr: { pr_url: "https://forge/x/42", pr_number: 42, state: "open", forge: "gh", repo: "x" }, count: 1 },
  );
  assert.match(withPr, /onclick="event\.stopPropagation\(\);ttOpenPr\('https:\/\/forge\/x\/42'\)"/);
  const approvedNoPr = ttReviewPrBadgesHtml(
    { review: { id: "guardian-1", name: "r", status: "approved", origin: "explicit", branches: [] }, count: 1 },
    null,
  );
  assert.match(approvedNoPr, /pr-placeholder/);
});

// ---------- click-engine wiring ----------

test("gotoReview navigation stops propagation so the task row's ttSelectTask doesn't also fire", () => {
  const m = boardSource.match(/CLICK_HANDLERS\.gotoReview = \(e, ds\) => \{([^}]*)\};/);
  assert.ok(m, "CLICK_HANDLERS.gotoReview handler not found");
  assert.match(m[1], /e\.stopPropagation\(\)/);
});

test("openReviewList (the +N affordance) switches to the Reviews tab without touching the selection", () => {
  const m = boardSource.match(/CLICK_HANDLERS\.openReviewList = \(e, ds\) => \{([^}]*)\};/);
  assert.ok(m, "CLICK_HANDLERS.openReviewList handler not found");
  assert.match(m[1], /e\.stopPropagation\(\)/);
  assert.match(m[1], /showTab\("reviews", true\)/);
  assert.doesNotMatch(m[1], /selectedGuardian/);
});

// ---------- gotoReview: immediate render + loading state ----------

test("gotoReview renders synchronously and flags an unloaded target with the loading placeholder", () => {
  const api = makeGotoReview({ findGuardian: () => false });
  api.gotoReview("guardian-9");
  const s = api.state();
  assert.equal(s.selectedGuardian, "guardian-9");
  assert.equal(s.revealedGuardianId, "guardian-9");
  assert.equal(s.reviewDetailLoading, "guardian-9");
  // all before any async work: showTab ran, and both surfaces re-rendered now
  assert.deepEqual(api.calls.showTab, ["reviews", true]);
  assert.equal(api.calls.renderReviews, 1);
  assert.equal(api.calls.renderReviewDetail, 1);
});

test("gotoReview does not set the loading flag when the target is already loaded", () => {
  const api = makeGotoReview({ findGuardian: (id) => id === "guardian-1" });
  api.gotoReview("guardian-1");
  assert.equal(api.state().reviewDetailLoading, null);
});

test("rapid clicks on different review badges leave the last-clicked review selected", () => {
  const api = makeGotoReview({ findGuardian: () => false });
  api.gotoReview("guardian-a");
  api.gotoReview("guardian-b");
  const s = api.state();
  assert.equal(s.selectedGuardian, "guardian-b");
  assert.equal(s.reviewDetailLoading, "guardian-b");
  assert.equal(api.calls.renderReviewDetail, 2);
});

test("renderReviewDetail shows the loading placeholder only for the pending navigation target", () => {
  const block = boardSource.slice(boardSource.indexOf("function renderReviewDetail()"));
  const head = block.slice(0, block.indexOf("const canReorder"));
  // placeholder for the pending target...
  assert.match(head, /selectedGuardian && reviewDetailLoading === selectedGuardian/);
  assert.match(head, /Loading review/);
  // ...and the plain prompt otherwise (including the nothing-selected case)
  assert.match(head, /Select a review\./);
  // the flag clears once the target's data is found
  assert.match(head, /reviewDetailLoading = null;/);
});

// ---------- pollReviews: latest-wins race guard (out-of-order completion) ----------

// The deferred lets the test park poll A inside its post-list Promise.all, i.e.
// at exactly the await point the second (pre-render) sequence guard protects.
function deferred() {
  let resolve;
  const promise = new Promise((r) => { resolve = r; });
  return { promise, resolve };
}

test("an older poll completing after a newer one abandons without rendering or overwriting guardians", async () => {
  const poll = makePollReviews();
  const promiseA = poll.pollReviews();  // seq 1 — its list fetch stays pending
  const promiseB = poll.pollReviews();  // seq 2

  // Queued so far: [0] A's list fetch, [1] B's list fetch. Resolve B's first.
  const [, fetchB] = poll.pendingFetches;
  resolveJson(fetchB, [{ id: "review-b", name: "b", status: "in_review" }]);
  await promiseB;
  assert.ok(poll.calls.renderReviews >= 1, "the newer poll renders");
  assert.deepEqual(poll.state().guardians, [{ id: "review-b", name: "b", status: "in_review" }]);
  const rendersAfterB = poll.calls.renderReviews;

  // Now A's stale list fetch completes, after the newer poll finished.
  const [fetchA] = poll.pendingFetches;
  resolveJson(fetchA, [{ id: "review-a", name: "a", status: "collecting" }]);
  await promiseA;
  // A must not render and must not clobber B's fresher `guardians`.
  assert.equal(poll.calls.renderReviews, rendersAfterB);
  assert.deepEqual(poll.state().guardians, [{ id: "review-b", name: "b", status: "in_review" }]);
});

test("a poll parked inside its branch refreshes does not render after being overtaken by a newer poll", async () => {
  const slowRefresh = deferred();
  const poll = makePollReviews({ slowRefresh: slowRefresh.promise });
  const promiseA = poll.pollReviews();  // seq 1

  // Resolve A's list fetch and let it run to (and into) its Promise.all.
  resolveJson(poll.pendingFetches[0], [{ id: "review-a", name: "a", status: "collecting" }]);
  await new Promise((r) => setImmediate(r));
  await new Promise((r) => setImmediate(r));

  const promiseB = poll.pollReviews();  // seq 2 — starts while A is parked
  const bListFetches = poll.pendingFetches.slice(1);
  const fetchB = bListFetches.find((f) => f.url === "/api/guardians");
  resolveJson(fetchB, [{ id: "review-b", name: "b", status: "in_review" }]);
  await promiseB;
  assert.ok(poll.calls.renderReviews >= 1, "the newer poll renders");
  assert.deepEqual(poll.state().guardians, [{ id: "review-b", name: "b", status: "in_review" }]);
  const rendersAfterB = poll.calls.renderReviews;

  // Release A's parked branch refresh — its sequence is now stale, so it must
  // hit the pre-render guard and abandon instead of re-rendering stale data.
  slowRefresh.resolve();
  await promiseA;
  assert.equal(poll.calls.renderReviews, rendersAfterB);
  assert.deepEqual(poll.state().guardians, [{ id: "review-b", name: "b", status: "in_review" }]);
});

test("a hash-navigated review that isn't loaded yet gets the loading placeholder instead of lingering on the previous selection", async () => {
  const poll = makePollReviews({ pendingHash: { tab: "reviews", guardianId: "review-new" } });
  const promise = poll.pollReviews();
  const [fetch] = poll.pendingFetches;
  resolveJson(fetch, [{ id: "review-old", name: "old", status: "collecting" }]);
  await promise;
  const s = poll.state();
  assert.equal(s.selectedGuardian, "review-new");
  assert.equal(s.reviewDetailLoading, "review-new");
  assert.equal(s.pendingHash, null);
});

test("a hash-navigated review that IS loaded doesn't flip the loading placeholder", async () => {
  const poll = makePollReviews({ pendingHash: { tab: "reviews", guardianId: "review-old" } });
  const promise = poll.pollReviews();
  const [fetch] = poll.pendingFetches;
  resolveJson(fetch, [{ id: "review-old", name: "old", status: "collecting" }]);
  await promise;
  assert.equal(poll.state().reviewDetailLoading, null);
});

// ---------- pollReviews: freshness stamp (board freshness-indicator honesty fix) ----------

test("a completed poll stamps the freshness indicator", async () => {
  const poll = makePollReviews();
  const promise = poll.pollReviews();
  resolveJson(poll.pendingFetches[0], [{ id: "review-a", name: "a", status: "in_review" }]);
  await promise;
  assert.equal(poll.calls.markUpdated, 1);
});

test("a poll abandoned by a newer one (RAL-382 guard) never stamps a time it did not render", async () => {
  const poll = makePollReviews();
  const promiseA = poll.pollReviews(); // seq 1 -- its list fetch stays pending
  const promiseB = poll.pollReviews(); // seq 2

  const [, fetchB] = poll.pendingFetches;
  resolveJson(fetchB, [{ id: "review-b", name: "b", status: "in_review" }]);
  await promiseB;
  assert.equal(poll.calls.markUpdated, 1, "the newer poll stamps once");

  const [fetchA] = poll.pendingFetches;
  resolveJson(fetchA, [{ id: "review-a", name: "a", status: "collecting" }]);
  await promiseA;
  assert.equal(poll.calls.markUpdated, 1, "the abandoned older poll must not stamp a second time");
});
