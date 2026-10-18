// Manual refresh controls go through guardedAction: no duplicate request while in flight or inside the cooldown, retry after failure.
import test from "node:test";
import assert from "node:assert/strict";
import { boardScript } from "./board-source.mjs";

const src = boardScript();
const a = src.indexOf("// RALPHUS-GUARDED-ACTION:BEGIN");
const b = src.indexOf("// RALPHUS-GUARDED-ACTION:END");
if (a === -1 || b < a) throw new Error("guarded-action markers not found");
const make = () => new Function(`${src.slice(a, b)}\nreturn { guardedAction, GUARDED_ACTION_COOLDOWN_MS };`)();

test("rapid repeat clicks while in flight issue one request", async () => {
  const { guardedAction } = make();
  let calls = 0;
  let release = () => {};
  const gate = new Promise((r) => { release = r; });
  const first = guardedAction("k", async () => { calls++; await gate; });
  assert.equal(await guardedAction("k", async () => { calls++; }), false);
  release();
  assert.equal(await first, true);
  assert.equal(calls, 1);
});

test("cooldown after completion, then a later click works", async () => {
  const { guardedAction, GUARDED_ACTION_COOLDOWN_MS } = make();
  let t = 1000;
  const now = () => t;
  let calls = 0;
  const fn = async () => { calls++; };
  assert.equal(await guardedAction("k", fn, now), true);
  assert.equal(await guardedAction("k", fn, now), false);
  t += GUARDED_ACTION_COOLDOWN_MS;
  assert.equal(await guardedAction("k", fn, now), true);
  assert.equal(calls, 2);
});

test("a failed action releases the guard so retry works; keys are independent", async () => {
  const { guardedAction, GUARDED_ACTION_COOLDOWN_MS } = make();
  let t = 0;
  const now = () => t;
  await assert.rejects(guardedAction("k", async () => { throw new Error("boom"); }, now));
  assert.equal(await guardedAction("other", async () => {}, now), true);
  t += GUARDED_ACTION_COOLDOWN_MS;
  assert.equal(await guardedAction("k", async () => {}, now), true);
});
