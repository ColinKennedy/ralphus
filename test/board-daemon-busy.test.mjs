// STRESS-006: the board backs off when the daemon sheds load with a `503`.
import { test } from "node:test";
import assert from "node:assert/strict";

import { busy } from "./board-daemon-busy.mjs";

const { daemonBusyDelayMs } = busy;

test("the first 503 waits Retry-After, at least one second", () => {
  assert.equal(daemonBusyDelayMs(1, 0, 0), 1000);
  assert.equal(daemonBusyDelayMs(1, 3, 0), 3000);
});

test("consecutive 503s double the wait up to 30 s", () => {
  assert.equal(daemonBusyDelayMs(2, 1, 0), 2000);
  assert.equal(daemonBusyDelayMs(3, 1, 0), 4000);
  assert.equal(daemonBusyDelayMs(6, 1, 0), 30000);
  assert.equal(daemonBusyDelayMs(50, 1, 0), 30000);
  assert.equal(daemonBusyDelayMs(2, 20, 0), 30000);
});

test("jitter adds at most a quarter, so tabs do not retry in lockstep", () => {
  assert.equal(daemonBusyDelayMs(1, 1, 0.999), 1250);
  assert.ok(daemonBusyDelayMs(6, 1, 0.5) > 30000);
  assert.ok(daemonBusyDelayMs(6, 1, 0.999) <= 37500);
});
