// The Triage tab's pure logic (librarian/assets/board/74-triage.js, the
// RALPHUS-TRIAGE-FLOW-LOGIC region): pool math, the cron parser that mirrors
// the daemon's `cron` crate (6 fields with seconds, weekday 1-7 from Sunday),
// next-firing with every-Nth occurrences, the threshold editor's effect line,
// and reading a drained review's triage type off its name. Sliced out of the
// real shipped source, like ./board-triage-confirm.mjs.

import test from "node:test";
import assert from "node:assert/strict";
import { boardScript } from "./board-source.mjs";

const BEGIN = "// RALPHUS-TRIAGE-FLOW-LOGIC:BEGIN";
const END = "// RALPHUS-TRIAGE-FLOW-LOGIC:END";

function load() {
  const html = boardScript();
  const from = html.indexOf(BEGIN);
  const to = html.indexOf(END);
  if (from === -1 || to === -1 || to < from) {
    throw new Error(`board-triage-flow: could not find the ${BEGIN} / ${END} markers. If this logic moved, move the markers with it.`);
  }
  // eslint-disable-next-line no-new-func -- evaluating the real shipped source is the point; see the header.
  return new Function(`${html.slice(from + BEGIN.length, to)}
    return { triageSplitKey, triagePoolMath, triageParseCron, triageCronError, triageCronNextAfter,
      triageScheduleNextFire, triageFormatNext, triageFormatIn, triageThresholdEffect, triageDrainedType };`)();
}

const T = load();
const utc = (s) => Date.parse(s);

test("splitKey separates a subproject composite key", () => {
  assert.deepEqual(T.triageSplitKey("proj::core"), { proj: "proj", sub: "core" });
  assert.deepEqual(T.triageSplitKey("proj"), { proj: "proj", sub: null });
});

test("pool math splits ready full batches from the pooled remainder", () => {
  assert.deepEqual(T.triagePoolMath({ project: "p", triage_type: "t", count: 7, threshold: 3 }, 0),
    { threshold: 3, fullBatches: 2, ready: 6, remainder: 1, isReady: true, stalled: false });
  const below = T.triagePoolMath({ project: "p", triage_type: "t", count: 2, threshold: 5 }, 0);
  assert.equal(below.ready, 0);
  assert.equal(below.remainder, 2);
  assert.equal(below.isReady, false);
});

test("a pool is stalled only with neither a threshold nor a schedule", () => {
  assert.equal(T.triagePoolMath({ project: "p", triage_type: "t", count: 2, threshold: null }, 0).stalled, true);
  assert.equal(T.triagePoolMath({ project: "p", triage_type: "t", count: 2, threshold: null }, 1).stalled, false);
});

test("cron needs the cron crate's 6 or 7 fields", () => {
  assert.equal(T.triageParseCron("0 9 * * MON"), null, "a classic 5-field cron is rejected, as the daemon does");
  assert.ok(T.triageParseCron("0 0 9 * * Mon"));
  assert.ok(T.triageParseCron("0 0 9 * * Mon 2027"));
  assert.match(T.triageCronError("0 9 * * MON"), /Needs 6 fields/);
  assert.equal(T.triageCronError("0 0 9 * * Mon"), "");
  assert.match(T.triageCronError("0 0 9 * * 8"), /isn't valid/);
});

test("weekday numbering runs 1-7 from Sunday, like the cron crate", () => {
  const sunday = T.triageParseCron("0 0 0 * * 1");
  const named = T.triageParseCron("0 0 0 * * SUN");
  assert.deepEqual([...sunday.dow], [1]);
  assert.deepEqual([...named.dow], [1]);
  // 2026-10-07 is a Wednesday; the next Sunday midnight is 2026-10-11.
  assert.equal(T.triageCronNextAfter(sunday, utc("2026-10-07T12:00:00Z")), utc("2026-10-11T00:00:00Z"));
});

test("next occurrence is strictly after the cursor and honours ranges and steps", () => {
  const spec = T.triageParseCron("0 */15 9-10 * * Mon-Fri");
  assert.equal(T.triageCronNextAfter(spec, utc("2026-10-07T09:00:00Z")), utc("2026-10-07T09:15:00Z"));
  assert.equal(T.triageCronNextAfter(spec, utc("2026-10-07T10:45:00Z")), utc("2026-10-08T09:00:00Z"));
  // Friday evening rolls over the weekend to Monday.
  assert.equal(T.triageCronNextAfter(spec, utc("2026-10-09T18:00:00Z")), utc("2026-10-12T09:00:00Z"));
});

test("next firing counts every-Nth occurrences from the daemon's cursor", () => {
  const sched = {
    id: 1, project: "p", triage_type: "t", cron_expr: "0 0 9 * * Mon",
    anchor_date_ms: utc("2026-09-01T00:00:00Z"), every_n: 2, occurrence_count: 3,
    last_checked_ms: utc("2026-09-28T09:00:00Z"),
  };
  // Occurrence 4 is Mon 5 Oct (fires: 4 % 2 == 0) but that is before "now"; occurrence 5 is
  // Mon 12 Oct (skipped), occurrence 6 is Mon 19 Oct (fires).
  assert.equal(T.triageScheduleNextFire(sched, utc("2026-10-07T00:00:00Z")), utc("2026-10-19T09:00:00Z"));
  assert.equal(T.triageScheduleNextFire({ ...sched, every_n: 1 }, utc("2026-10-07T00:00:00Z")), utc("2026-10-12T09:00:00Z"));
});

test("a never-checked schedule counts from its anchor", () => {
  const sched = {
    id: 1, project: "p", triage_type: "t", cron_expr: "0 0 0 1 * *",
    anchor_date_ms: utc("2026-01-01T00:00:00Z"), every_n: 3, occurrence_count: 0, last_checked_ms: null,
  };
  // Monthly on the 1st after the anchor: Feb (1), Mar (2), Apr (3, fires), ... Oct (9, fires) — first one after now.
  assert.equal(T.triageScheduleNextFire(sched, utc("2026-07-15T00:00:00Z")), utc("2026-10-01T00:00:00Z"));
});

test("formatting reads in UTC", () => {
  assert.equal(T.triageFormatNext(utc("2026-10-12T09:00:00Z")), "Mon 12 Oct 09:00 UTC");
  const now = utc("2026-10-07T00:00:00Z");
  assert.equal(T.triageFormatIn(now + 4 * 60000, now), "in 4m");
  assert.equal(T.triageFormatIn(now + (3 * 60 + 5) * 60000, now), "in 3h 5m");
  assert.equal(T.triageFormatIn(now + (52 * 60) * 60000, now), "in 2d 4h");
  assert.equal(T.triageFormatIn(now - 1000, now), "now");
});

test("threshold effect never promises an immediate drain", () => {
  const ready = T.triageThresholdEffect(7, "3", 0);
  assert.equal(ready.ok, true);
  assert.match(ready.text, /2 full batches/);
  assert.match(ready.text, /next finishes, or use Drain now/);
  assert.match(T.triageThresholdEffect(2, "5", 0).text, /Drains once 5 cells are pooled \(2 now\)/);
  assert.match(T.triageThresholdEffect(2, "", 1).text, /only a cron schedule/);
  assert.match(T.triageThresholdEffect(2, "", 0).text, /nothing will drain/);
  assert.equal(T.triageThresholdEffect(2, "0", 0).ok, false);
  assert.equal(T.triageThresholdEffect(2, "2.5", 0).ok, false);
});

test("a drained review's type is read off its name, longest registered type first", () => {
  const types = ["bug", "bug-fix", "feature"];
  assert.equal(T.triageDrainedType("triage-feature", types), "feature");
  assert.equal(T.triageDrainedType("triage-bug-core", types), "bug");
  assert.equal(T.triageDrainedType("triage-bug-fix", types), "bug-fix");
  assert.equal(T.triageDrainedType("triage-unknown", types), null);
  assert.equal(T.triageDrainedType("my-review", types), null);
});
