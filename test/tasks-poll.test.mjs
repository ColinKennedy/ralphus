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
import { selTransition } from "./board-sel-transition.mjs";

// ---------- updateCounter / pollTasks: shared in-flight request + ticket ----------

test("updateCounter: concurrent calls share one in-flight /api/task-index request instead of firing a duplicate", async () => {
  const poll = makeTasksPoll();
  const promiseA = poll.updateCounter();
  const promiseB = poll.updateCounter();
  assert.equal(poll.pendingFetches.length, 1, "the second call must not fire its own duplicate fetch");
  assert.equal(poll.pendingFetches[0].url, "/api/task-index", "the counter reads the compact index, not the full board");

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

test("pollTasks and updateCounter read different endpoints, so neither is starved by the other's payload", async () => {
  // These two deliberately no longer share a request. `updateCounter` reads
  // the compact `/api/task-index` (684KB) for the chrome counter, while
  // `pollTasks` reads the full `/api/tasks` board (6.2MB) it needs to render
  // per-cell detail. They also no longer run on the same tab: `tick()` skips
  // `updateCounter` on Squads/Tasks precisely because those tabs' own polls
  // already refresh the counter from the response they fetch anyway.
  const poll = makeTasksPoll();
  const counterPromise = poll.updateCounter();
  const tasksPromise = poll.pollTasks();
  assert.deepEqual(
    poll.pendingFetches.map((f) => f.url).sort(),
    ["/api/task-index", "/api/tasks"],
    "each function fetches its own endpoint",
  );

  resolveJson(poll.pendingFetches[0], { daemon: { running: 3, max_concurrent: 4 }, squads: [{ id: "s-index" }] });
  resolveJson(poll.pendingFetches[1], { daemon: { running: 3, max_concurrent: 4 }, squads: [{ id: "s-fresh" }] });
  await Promise.all([counterPromise, tasksPromise]);
  // `pollTasks` holds the later ticket, so the full board's data is what
  // survives in `squads` -- the counter's compact copy must not clobber it.
  assert.deepEqual(poll.state().squads, [{ id: "s-fresh" }]);
});

test("updateCounter starting after an in-flight pollTasks must not cancel its render", async () => {
  // The regression behind "the status badge only updates if I click the squad
  // away and back": `updateCounter` used to claim a `tasksPollSeq` ticket of
  // its own. Because it refreshes the counter but renders nothing, claiming
  // one superseded the in-flight `pollTasks`, which then abandoned itself on
  // the ticket check and skipped its render -- leaving fresh data in `squads`
  // behind a stale DOM, with no further event scheduled to repaint it.
  //
  // This is the exact ordering `tick()` and an SSE-driven refresh produce
  // when they overlap, and it is the reverse of the ordering the
  // "share one in-flight request across functions" test above covers.
  const poll = makeTasksPoll();
  const tasksPromise = poll.pollTasks();       // claims the render ticket
  const counterPromise = poll.updateCounter(); // must observe it, not claim it

  // `/api/tasks` (pollTasks) resolves last, so if updateCounter had claimed a
  // ticket it would have superseded the render before it ever ran.
  resolveJson(poll.pendingFetches[1], { daemon: { running: 2, max_concurrent: 4 }, squads: [{ id: "s-1", state: "cancelled" }] });
  await counterPromise;
  resolveJson(poll.pendingFetches[0], { daemon: { running: 2, max_concurrent: 4 }, squads: [{ id: "s-1", state: "cancelled" }] });
  await Promise.all([tasksPromise, counterPromise]);

  assert.deepEqual(poll.state().squads, [{ id: "s-1", state: "cancelled" }]);
  assert.equal(poll.calls.renderAll, 1, "the pollTasks render must survive an overlapping updateCounter");
});

// ---------- prompt-text cache: the details pane must not blank on every poll ----------

test("cached prompt text is re-applied to the fresh rows, and a standing poll neither refetches nor double-renders", async () => {
  // `/api/tasks` omits prompt text, so every poll hands back rows whose
  // prompt fields are null. Without a cache applied before the first render,
  // the details pane paints empty and only fills in once a follow-up fetch
  // lands -- a visible blank-and-repaint on every single refresh.
  const poll = makeTasksPoll({ selectedSquadId: "s-1" });

  // First poll: the squad is now known, so the prompt fetch can run.
  const first = poll.pollTasks();
  resolveJson(poll.pendingFetches[0], {
    daemon: { running: 0, max_concurrent: 0 },
    squads: [{ id: "s-1", tasks: [{ cells: [{ prompt: null, system_prompt: null, proof: [] }], proof: [] }] }],
  });
  await first;

  // The follow-up per-squad fetch carries the real text.
  const detail = poll.pendingFetches.find((f) => f.url.startsWith("/api/squads/"));
  assert.ok(detail, "a squad-detail fetch must be issued once the squad is known");
  resolveJson(detail, { id: "s-1", tasks: [{ cells: [{ prompt: "P", system_prompt: "SP", proof: [] }], proof: [] }] });
  await new Promise((r) => setTimeout(r, 0));
  assert.equal(poll.state().promptCacheSquadId, "s-1", "the text is cached against its squad");

  const rendersAfterLoad = poll.calls.renderAll;
  const fetchesAfterLoad = poll.pendingFetches.length;

  // Second poll: fresh rows, prompt fields null again.
  const second = poll.pollTasks();
  resolveJson(poll.pendingFetches[fetchesAfterLoad], {
    daemon: { running: 0, max_concurrent: 0 },
    squads: [{ id: "s-1", tasks: [{ cells: [{ prompt: null, system_prompt: null, proof: [] }], proof: [] }] }],
  });
  await second;

  const cell = poll.state().squads[0].tasks[0].cells[0];
  assert.equal(cell.prompt, "P", "cached prompt must be restored before the render, not after");
  assert.equal(cell.system_prompt, "SP", "cached system prompt must be restored too");
  assert.equal(poll.pendingFetches.length, fetchesAfterLoad + 1,
    "a standing poll must not re-fetch the squad detail -- only the board request");
  assert.equal(poll.calls.renderAll, rendersAfterLoad + 1,
    "exactly one render per poll: a second one is the blank-then-repaint flicker");
});

test("selecting a squad fetches its prompt text immediately, not on the next poll", async () => {
  // The fetch used to be kicked off only at the tail of `pollTasks`, so a
  // selection made while nothing else was happening sat empty until the next
  // poll -- on a quiet daemon, the 60s reconciliation tick. `renderDetails`
  // calls `syncPromptCache`, and every selection path renders, so picking a
  // cell now starts the request straight away.
  const poll = makeTasksPoll({ selectedSquadId: "s-1" });
  const first = poll.pollTasks();
  resolveJson(poll.pendingFetches[0], {
    daemon: { running: 0, max_concurrent: 0 },
    squads: [{ id: "s-1", tasks: [{ cells: [{ prompt: null, proof: [] }], proof: [] }] }],
  });
  await first;
  // Drain the load the poll itself started, so the cache is cold again.
  const initial = poll.pendingFetches.find((f) => f.url.startsWith("/api/squads/"));
  resolveJson(initial, { id: "s-1", tasks: [{ cells: [{ prompt: "P", proof: [] }], proof: [] }] });
  await new Promise((r) => setTimeout(r, 0));
  poll.invalidateTasksFetch(); // e.g. a mutation cleared it
  assert.equal(poll.state().promptCacheSquadId, null);

  const before = poll.pendingFetches.length;
  poll.syncPromptCache();      // what a selection triggers, with no poll involved
  assert.equal(poll.pendingFetches.length, before + 1,
    "a selection whose squad is not cached must issue the fetch itself");
  assert.ok(poll.pendingFetches[before].url.startsWith("/api/squads/"),
    "and it must be the per-squad detail request");

  resolveJson(poll.pendingFetches[before], { id: "s-1", tasks: [{ cells: [{ prompt: "P2", proof: [] }], proof: [] }] });
  await new Promise((r) => setTimeout(r, 0));
  assert.equal(poll.state().promptCacheSquadId, "s-1");
  assert.ok(poll.calls.renderDetails > 0, "the pane repaints once the text arrives");

  // A second call with the cache warm must not fetch again.
  const warm = poll.pendingFetches.length;
  poll.syncPromptCache();
  assert.equal(poll.pendingFetches.length, warm, "a cached squad issues no further request");
});

test("invalidateTasksFetch drops the prompt cache, so an edited prompt is refetched", async () => {
  const poll = makeTasksPoll({ selectedSquadId: "s-1" });
  const first = poll.pollTasks();
  resolveJson(poll.pendingFetches[0], {
    daemon: { running: 0, max_concurrent: 0 },
    squads: [{ id: "s-1", tasks: [{ cells: [{ prompt: null, proof: [] }], proof: [] }] }],
  });
  await first;
  const detail = poll.pendingFetches.find((f) => f.url.startsWith("/api/squads/"));
  resolveJson(detail, { id: "s-1", tasks: [{ cells: [{ prompt: "old", proof: [] }], proof: [] }] });
  await new Promise((r) => setTimeout(r, 0));
  assert.equal(poll.state().promptCacheSquadId, "s-1");

  poll.invalidateTasksFetch();
  assert.equal(poll.state().promptCacheSquadId, null,
    "a squad mutation can rewrite a prompt, so the cached text must not survive it");
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

// ---------- invalidateTasksFetch: RAL-406 stale-status-after-mutation race ----------
//
// The dedup above (fetchTasksShared) has a sharp edge: a poll that began
// *before* a mutation (e.g. cancelling a squad) can still be in flight when
// the mutation commits server-side. Without invalidation, the mutation's
// own post-commit tick()/pollTasks() call shares that same pre-mutation
// in-flight request via fetchTasksShared() instead of firing a fresh one --
// so even the "latest ticket" caller ends up rendering pre-mutation status,
// and it stays stuck until some unrelated later poll (e.g. a tab switch)
// finally issues a fresh request. See post()/del() in 20-util.js (RAL-406)
// for the call sites that invoke invalidateTasksFetch() on every
// squad-scoped mutation.

test("invalidateTasksFetch: a poll started after a squad mutation gets its own fresh /api/tasks request instead of piggybacking on the pre-mutation one (RAL-406)", async () => {
  const poll = makeTasksPoll();
  // Some poll already in flight (the 60s reconciliation tick, an SSE
  // refresh, ...) when the user cancels a squad.
  const stalePromise = poll.pollTasks(); // ticket 1
  assert.equal(poll.pendingFetches.length, 1);

  // The cancel mutation invalidates the shared in-flight request the moment
  // it commits server-side, then the post-cancel tick() polls again.
  poll.invalidateTasksFetch();
  const freshPromise = poll.pollTasks(); // ticket 2
  assert.equal(poll.pendingFetches.length, 2, "the post-mutation poll must not share the pre-mutation in-flight request");

  // The fresh, post-mutation request resolves first with the correct
  // "cancelled" status; the stale pre-mutation one resolves after with the
  // old "running" status but must lose regardless of arrival order.
  resolveJson(poll.pendingFetches[1], { daemon: { running: 0, max_concurrent: 2 }, squads: [{ id: "s-1", state: "cancelled" }] });
  await freshPromise;
  resolveJson(poll.pendingFetches[0], { daemon: { running: 1, max_concurrent: 2 }, squads: [{ id: "s-1", state: "running" }] });
  await stalePromise;

  assert.deepEqual(poll.state().squads, [{ id: "s-1", state: "cancelled" }], "the fresh post-mutation data must win, not the stale pre-mutation response");
});

test("invalidateTasksFetch: a no-op when nothing is in flight -- the next poll behaves normally", async () => {
  const poll = makeTasksPoll();
  poll.invalidateTasksFetch();
  const promise = poll.pollTasks();
  assert.equal(poll.pendingFetches.length, 1);
  resolveJson(poll.pendingFetches[0], { daemon: { running: 0, max_concurrent: 0 }, squads: [{ id: "s-1" }] });
  await promise;
  assert.deepEqual(poll.state().squads, [{ id: "s-1" }]);
});

test("invalidateTasksFetch: does not clobber a newer in-flight request that already replaced the stale one before it settles", async () => {
  const poll = makeTasksPoll();
  const stalePromise = poll.pollTasks(); // ticket 1, request A
  poll.invalidateTasksFetch();
  const freshPromise = poll.pollTasks(); // ticket 2, request B -- distinct from A

  // Resolve the stale request A now -- its own fetchTasksShared cleanup
  // must not evict request B's still-in-flight slot.
  resolveJson(poll.pendingFetches[0], { daemon: { running: 1, max_concurrent: 2 }, squads: [{ id: "s-1", state: "running" }] });
  await stalePromise;

  // A third caller arriving now (before B settles) must still share B, not
  // fire a redundant third request.
  const thirdPromise = poll.pollTasks(); // ticket 3, shares request B
  assert.equal(poll.pendingFetches.length, 2, "request A settling must not force a spurious third fetch while B is still in flight");

  resolveJson(poll.pendingFetches[1], { daemon: { running: 0, max_concurrent: 2 }, squads: [{ id: "s-1", state: "cancelled" }] });
  await Promise.all([freshPromise, thirdPromise]);
  assert.deepEqual(poll.state().squads, [{ id: "s-1", state: "cancelled" }]);
});

// ---------- RAL-419: selection caches riding the poll ----------

function squadView(id, tasks = []) {
  return { id, label: id, state: "done", created_at_ms: 1, tasks };
}

test("RAL-419: a refresh prunes dead-squad entries and reconciles the live selection", async () => {
  const poll = makeTasksPoll({ selectedSquadId: "s-alive" });
  poll.state().squadSelCache["s-deleted"] = { kind: "cell", taskIdx: 0, cellIdx: 0, proofIdx: -1 };
  poll.state().squadSelCache["s-alive"] = { kind: "squad", taskIdx: 0, cellIdx: 0, proofIdx: -1 };
  poll.state().squadNodeCache["s-deleted"] = ["cell:0:0:-1"];
  const promise = poll.pollTasks();
  resolveJson(poll.pendingFetches[0], { daemon: { running: 0, max_concurrent: 1 }, squads: [squadView("s-alive")] });
  await promise;
  const s = poll.state();
  assert.equal(s.squadSelCache["s-deleted"], undefined, "the deleted squad's entry is pruned");
  assert.equal(s.squadNodeCache["s-deleted"], undefined, "the deleted squad's node keys are pruned too");
  assert.deepEqual(s.squadSelCache["s-alive"], { kind: "squad", taskIdx: 0, cellIdx: 0, proofIdx: -1 }, "the surviving squad's entry stays");
  assert.deepEqual(poll.calls.pruned[0], ["s-alive"], "prune is fed the fresh full squad-id list");
});

test("RAL-419: a refresh that removes the selected node drops the live selection to its nearest surviving parent and clears the stale entry", async () => {
  const poll = makeTasksPoll({ selectedSquadId: "s-alive" });
  const seed = poll.state().sel;
  seed.kind = "cell"; seed.taskIdx = 0; seed.cellIdx = 7; seed.proofIdx = -1; // dangling: cell 7 does not exist
  poll.state().squadSelCache["s-alive"] = { kind: "cell", taskIdx: 0, cellIdx: 7, proofIdx: -1 };
  poll.state().squadNodeCache["s-alive"] = ["cell:0:7:-1"];
  const squad = squadView("s-alive", [{
    name: "t0", project: "p", agent: null, model: null, state: "done", soloed: false,
    cells: [{ id: "c0", cwd: ".", agent: "claude", model: null, state: "done" }],
  }]);
  const promise = poll.pollTasks();
  resolveJson(poll.pendingFetches[0], { daemon: { running: 0, max_concurrent: 1 }, squads: [squad] });
  await promise;
  const s = poll.state();
  assert.deepEqual(s.sel, { kind: "task", taskIdx: 0, cellIdx: -1, proofIdx: -1 }, "falls back to the nearest surviving parent (the task)");
  assert.equal(s.squadSelCache["s-alive"], undefined, "the stale cache entry is cleared");
  assert.equal(s.squadNodeCache["s-alive"], undefined, "the stale node keys are cleared");
  assert.equal(s.nodeMultiSel.size, 0, "no dead multi keys ride along");
});

test("RAL-419: no selection + squads present routes to restoreInitialSquadSelection (last-focused restore)", async () => {
  const poll = makeTasksPoll({
    selectedSquadId: null,
    selectionOps: { applySquadFocus: (id) => { poll.calls.appliedFocus.push(id); } },
  });
  const promise = poll.pollTasks();
  resolveJson(poll.pendingFetches[0], { daemon: { running: 0, max_concurrent: 1 }, squads: [squadView("s-1")] });
  await promise;
  assert.deepEqual(poll.calls.appliedFocus, ["s-1"], "first paint with squads but no selection must restore the initial squad");
});

test("RAL-419: a resolved pending hash updates the per-squad cache after resolution", async () => {
  const target = squadView("s-9", [{ name: "t0", project: "p", agent: null, model: null, state: "done", soloed: false, cells: [{ id: "c0", cwd: ".", agent: "claude", model: null, state: "done" }] }]);
  let settledWith = null;
  const ops = {
    squadForPendingHash: (ph) => (ph.squadId === target.id ? target : undefined),
    selForPendingHash: (ph, squad) => { settledWith = squad; return { kind: "cell", taskIdx: 0, cellIdx: 0, proofIdx: -1 }; },
  };
  const poll = makeTasksPoll({ pendingHash: { tab: "squads", squadId: target.id }, selectedSquadId: null, selectionOps: ops });
  const promise = poll.pollTasks();
  resolveJson(poll.pendingFetches[0], { daemon: { running: 0, max_concurrent: 1 }, squads: [target] });
  await promise;
  const s = poll.state();
  assert.equal(settledWith, target, "settlement must pass the resolved squad view");
  assert.deepEqual(s.sel, { kind: "cell", taskIdx: 0, cellIdx: 0, proofIdx: -1 }, "the hash's authoritative selection drives the details pane");
  assert.deepEqual(s.squadSelCache[target.id], { kind: "cell", taskIdx: 0, cellIdx: 0, proofIdx: -1 }, "the cache is updated from the resolved selection, not before it");
});

test("RAL-419: a pending hash whose target squad never loads falls back to the initial restore path", async () => {
  const poll = makeTasksPoll({
    pendingHash: { tab: "squads", squadId: "s-gone" },
    selectedSquadId: null,
    selectionOps: { applySquadFocus: (id) => { poll.calls.appliedFocus.push(id); } },
  });
  const promise = poll.pollTasks();
  resolveJson(poll.pendingFetches[0], { daemon: { running: 0, max_concurrent: 1 }, squads: [squadView("s-1")] });
  await promise;
  assert.deepEqual(poll.calls.appliedFocus, ["s-1"], "falls back to the initial restore path");
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
