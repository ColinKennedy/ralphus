// Coverage for RAL-390 "board polls can finish out of order" and its
// follow-up "board polls can flood and starve each other out":
//
// - `updateCounter`/`pollTasks` (Squads tab) share one `tasksPollSeq` ticket --
//   whichever of them started last is the only one allowed to write `squads`/
//   the daemon-status counter, regardless of which one's response arrives
//   first over the network;
// - the two ALSO share one in-flight `/api/tasks` request (`fetchTasksShared`)
//   -- overlapping callers piggyback on the same network round trip instead of
//   each firing a redundant duplicate. This is the actual fix for the observed
//   "Squads/Tasks tab never updates:" once `/api/tasks` is slow (a large squad
//   history makes it multi-second) and squad-state events keep firing faster
//   than that, the ticket guard alone made EVERY response arrive stale --
//   nothing ever rendered. Deduplicating the requests removes the flood that
//   caused that in the first place;
// - `pollWhoAmI` carries its own `whoAmIPollSeq` ticket, so a slow/stale
//   "not admin" response can't land after a fresh "is admin" one and blink
//   the admin-only tabs back out.
//
// The ticket-only guard is the same latest-wins pattern RAL-382 already
// proved out for `pollReviews` (see ./review-badge-nav.test.mjs); the
// dedup-on-top-of-it is new here and belongs on `pollReviews` too (same
// symptom reported on the Reviews tab) -- see ./review-badge-nav.test.mjs
// for that coverage.
//
// Run with `npm test` (node --test). See ./board-tasks-poll.mjs for how the
// regions are loaded out of the real board chunks.

import test from "node:test";
import assert from "node:assert/strict";
import { makeTasksPoll, makeWhoAmIPoll, resolveJson } from "./board-tasks-poll.mjs";

// ---------- updateCounter / pollTasks: shared in-flight request + ticket ----------

test("updateCounter: concurrent calls share one in-flight /api/tasks request instead of firing a duplicate", async () => {
  const poll = makeTasksPoll();
  const promiseA = poll.updateCounter();
  const promiseB = poll.updateCounter();
  assert.equal(poll.pendingFetches.length, 1, "the second call must not fire its own duplicate fetch");

  resolveJson(poll.pendingFetches[0], { daemon: { running: 1, max_concurrent: 2 }, squads: [{ id: "s-1" }] });
  await Promise.all([promiseA, promiseB]);
  assert.deepEqual(poll.state().squads, [{ id: "s-1" }]);
});

test("pollTasks: concurrent calls share one in-flight fetch, and only the later-started call renders", async () => {
  const poll = makeTasksPoll();
  const promiseA = poll.pollTasks(); // ticket 1
  const promiseB = poll.pollTasks(); // ticket 2 -- coalesces onto A's fetch, not a new one
  assert.equal(poll.pendingFetches.length, 1, "both calls must share one /api/tasks request");

  resolveJson(poll.pendingFetches[0], { daemon: { running: 0, max_concurrent: 0 }, squads: [{ id: "s-1" }] });
  await Promise.all([promiseA, promiseB]);
  assert.deepEqual(poll.state().squads, [{ id: "s-1" }]);
  // Both subscribed to the same resolved data, but only ticket 2 (the latest
  // at the time it landed) is allowed to actually apply/render it -- ticket 1
  // must recognize it's been superseded and skip, avoiding a redundant
  // duplicate render of the identical data.
  assert.equal(poll.calls.renderAll, 1, "only the latest-ticket caller renders, not both");
});

test("pollTasks and updateCounter share one in-flight /api/tasks request across functions too", async () => {
  // Mirrors the real collision: an SSE-pushed pollTasks() and a tick()-driven
  // updateCounter() (or vice versa) can both fire for the Squads tab at once,
  // since both hit /api/tasks and write the same `squads` global.
  const poll = makeTasksPoll();
  const counterPromise = poll.updateCounter();
  const tasksPromise = poll.pollTasks();
  assert.equal(poll.pendingFetches.length, 1, "the two functions must share one /api/tasks request, not fire one each");

  resolveJson(poll.pendingFetches[0], { daemon: { running: 3, max_concurrent: 4 }, squads: [{ id: "s-fresh" }] });
  await Promise.all([counterPromise, tasksPromise]);
  assert.deepEqual(poll.state().squads, [{ id: "s-fresh" }]);
});

test("a request after the previous one has already settled is not coalesced -- dedup clears once the in-flight request lands", async () => {
  const poll = makeTasksPoll();
  const promiseA = poll.updateCounter();
  resolveJson(poll.pendingFetches[0], { daemon: { running: 0, max_concurrent: 0 }, squads: [{ id: "s-old" }] });
  await promiseA;

  const promiseB = poll.updateCounter();
  assert.equal(poll.pendingFetches.length, 2, "a poll started after the prior request settled must get its own fresh fetch");
  resolveJson(poll.pendingFetches[1], { daemon: { running: 1, max_concurrent: 1 }, squads: [{ id: "s-new" }] });
  await promiseB;
  assert.deepEqual(poll.state().squads, [{ id: "s-new" }]);
});

test("a single normal poll still updates squads and the updated-clock label (no regression from the guard)", async () => {
  const poll = makeTasksPoll();
  const promise = poll.pollTasks();
  const [fetch] = poll.pendingFetches;
  resolveJson(fetch, { daemon: { running: 1, max_concurrent: 2 }, squads: [{ id: "s-1" }] });
  await promise;
  assert.deepEqual(poll.state().squads, [{ id: "s-1" }]);
  assert.match(poll.els.updated.textContent, /^updated /);
  assert.equal(poll.els.conn.className, "dot on");
});

// ---------- pollWhoAmI: its own ticket ----------

test("pollWhoAmI: a stale non-admin response completing after a fresher admin one does not revert currentUserIsAdmin", async () => {
  const poll = makeWhoAmIPoll();
  const promiseA = poll.pollWhoAmI(); // ticket 1 -- will resolve is_admin:false, but late
  const promiseB = poll.pollWhoAmI(); // ticket 2 -- resolves is_admin:true, first

  const [, fetchB] = poll.pendingFetches;
  resolveJson(fetchB, { name: "colin", is_admin: true });
  await promiseB;
  assert.equal(poll.state().currentUserIsAdmin, true);

  const [fetchA] = poll.pendingFetches;
  resolveJson(fetchA, { name: "colin", is_admin: false });
  await promiseA;
  assert.equal(poll.state().currentUserIsAdmin, true, "the stale response must not flip admin status back off");
});

test("pollWhoAmI: a single normal poll still resolves identity and admin status (no regression from the guard)", async () => {
  const poll = makeWhoAmIPoll();
  const promise = poll.pollWhoAmI();
  const [fetch] = poll.pendingFetches;
  resolveJson(fetch, { name: "colin", is_admin: true });
  await promise;
  const s = poll.state();
  assert.equal(s.currentUserName, "colin");
  assert.equal(s.currentUserIsAdmin, true);
  assert.equal(s.whoAmIResolved, true);
  assert.equal(poll.calls.applyAdminTabVisibility, 1);
});
