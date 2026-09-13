// Coverage for RAL-406 "stale squad status after cancel/other mutations":
// `post`/`del` (librarian/assets/board/20-util.js) invalidate the shared
// `/api/tasks` in-flight request (see ./tasks-poll.test.mjs for that side of
// the fix) for every squad-scoped mutation, so the `tick()` refresh that
// almost always follows can't land pre-mutation data by piggybacking on a
// request sent before the mutation committed server-side.
//
// Run with `npm test` (node --test). See ./board-post-del.mjs for how the
// region is loaded out of the real board chunks.

import test from "node:test";
import assert from "node:assert/strict";
import { makePostDel } from "./board-post-del.mjs";

test("post: a squad-scoped mutation invalidates the shared tasks fetch before sending the request", async () => {
  const { post, calls } = makePostDel();
  await post("/api/squads/s-1/cancel");
  assert.equal(calls.invalidateTasksFetch, 1);
  assert.equal(calls.fetches.length, 1);
  assert.equal(calls.fetches[0].path, "/api/squads/s-1/cancel");
});

test("post: covers every squad-mutation shape -- activate, retry, restart, set-status, edit", async () => {
  const { post, calls } = makePostDel();
  await post("/api/squads/s-1/activate");
  await post("/api/squads/s-1/retry");
  await post("/api/squads/s-1/tasks/0/restart");
  await post("/api/squads/s-1/set-status", { state: "cancelled" });
  await post("/api/squads/s-1/edit", { label: "renamed" });
  assert.equal(calls.invalidateTasksFetch, 5);
});

test("post: an unrelated mutation (not squad-scoped) does not invalidate the tasks fetch", async () => {
  const { post, calls } = makePostDel();
  await post("/api/hidden/squads/s-1");
  await post("/api/watches?uri=foo");
  await post("/api/hidden/reviews/g-1");
  assert.equal(calls.invalidateTasksFetch, 0);
});

test("del: squad deletion invalidates the shared tasks fetch", async () => {
  const { del, calls } = makePostDel();
  await del("/api/squads/s-1");
  assert.equal(calls.invalidateTasksFetch, 1);
});

test("del: an unrelated delete (not squad-scoped) does not invalidate the tasks fetch", async () => {
  const { del, calls } = makePostDel();
  await del("/api/hidden/squads/s-1");
  await del("/api/hidden/reviews/g-1");
  assert.equal(calls.invalidateTasksFetch, 0);
});
