// Coverage for the board freshness-indicator honesty fix (daemon
// contention/board-freshness investigation):
//
// - `updateCounter` no longer swallows a failed `/api/tasks` fetch silently --
//   it marks the connection dot off and replaces the "#updated" stamp with
//   "daemon unreachable" instead of leaving a stale time on screen next to
//   data that never loaded;
// - `pollTasksTab` (Tasks tab) now stamps "#updated" on both its render exits
//   (the hash-restore path and the plain background-poll path), so the tab no
//   longer shows a dashed-out "-" forever or an unrelated time left over from
//   another tab's fetch;
// - `pollReviews` (Reviews tab) stamping is covered in ./review-badge-nav.test.mjs
//   alongside its own RAL-382 sequence guard;
// - `showTab` clears the stamp back to "-" on every tab switch, so a value
//   from the previous tab can never be misread as belonging to the new one.
//
// Run with `npm test` (node --test). See ./board-freshness.mjs for how the
// regions are loaded out of the real board chunks.

import test from "node:test";
import assert from "node:assert/strict";
import { boardSource, makeUpdateCounter, makePollTasksTab, resolveJson, rejectFetch } from "./board-freshness.mjs";

// ---------- updateCounter: no longer swallows a failed fetch ----------

test("updateCounter: a rejected /api/tasks fetch marks the board unreachable instead of failing silently", async () => {
  const uc = makeUpdateCounter();
  const promise = uc.updateCounter();
  rejectFetch(uc.pendingFetches[0]);
  await promise;
  assert.equal(uc.els.conn.className, "dot off");
  assert.equal(uc.els.updated.textContent, "daemon unreachable");
});

test("updateCounter: a successful fetch never marks the board unreachable (the tab-specific poller called right after owns the stamp)", async () => {
  const uc = makeUpdateCounter();
  const promise = uc.updateCounter();
  resolveJson(uc.pendingFetches[0], { daemon: { running: 1, max_concurrent: 2 }, squads: [{ id: "s-1" }] });
  await promise;
  assert.equal(uc.els.conn, undefined, "success path never touches the connection dot");
  assert.equal(uc.els.updated, undefined, "success path leaves the stamp for the tab-specific poller that runs right after it");
  assert.deepEqual(uc.state().squads, [{ id: "s-1" }]);
});

// ---------- pollTasksTab: stamps #updated on both render exits ----------

test("pollTasksTab: the normal (non-hash) refresh path stamps #updated", async () => {
  const pt = makePollTasksTab();
  const promise = pt.pollTasksTab();
  resolveJson(pt.pendingFetches[0], {});
  await promise;
  assert.match(pt.els.updated.textContent, /^updated /);
  assert.equal(pt.calls.renderTasksTab, 1);
});

test("pollTasksTab: the pendingHash restore path also stamps #updated", async () => {
  const pt = makePollTasksTab({ pendingHash: { tab: "tasks", uri: "ralphus:/SQUAD[x]" } });
  const promise = pt.pollTasksTab();
  resolveJson(pt.pendingFetches[0], {});
  await promise;
  assert.match(pt.els.updated.textContent, /^updated /);
  assert.equal(pt.calls.ttScrollSelectionIntoView, 1);
  assert.equal(pt.state().pendingHash, null);
});

test("pollTasksTab: a failed /api/pull-requests/index fetch still stamps #updated (transient, not a daemon-down signal)", async () => {
  const pt = makePollTasksTab();
  const promise = pt.pollTasksTab();
  rejectFetch(pt.pendingFetches[0]);
  await promise;
  assert.match(pt.els.updated.textContent, /^updated /);
});

// ---------- showTab: clears the stamp so it can't be misread across tabs ----------

test("showTab resets #updated to the placeholder before the new tab has fetched anything", () => {
  const body = boardSource.slice(
    boardSource.indexOf("function showTab(name, push = false) {"),
    boardSource.indexOf("function showTab(name, push = false) {") + 1500,
  );
  const setsTab = body.indexOf("tab = name;");
  const resetsStamp = body.indexOf('byId("updated").textContent = "—";');
  const startsTabLoop = body.indexOf("for (const t of TABS)");
  assert.ok(setsTab !== -1 && resetsStamp !== -1 && startsTabLoop !== -1, "expected statements not found in showTab");
  assert.ok(
    setsTab < resetsStamp && resetsStamp < startsTabLoop,
    "the stamp must reset after the tab switches and before the tab pages toggle, so it never shows a value belonging to the old tab",
  );
});
