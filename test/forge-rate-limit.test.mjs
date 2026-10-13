// Coverage for the review inspector's "forge rate limit" section: what
// `forgeRateLimitRowsHtml` shows for the daemon's `forge-status` reply.
//
// - quota rows state remaining/limit and when the window reopens, and flag a
//   nearly-spent quota with the "delayed" colour (a timer the daemon waits out);
// - an active backoff is shown as a pause with its resume time, and pushes the
//   "next CI check" row to match what the daemon reports;
// - a branch with no PR, no quota reading or no forge client still renders a
//   readable explanation instead of an empty section;
// - a PR whose last poll errored shows that error in the failure colour;
// - every row carries a data-tip tooltip (UI Tooltip Rule), and daemon-supplied
//   text is HTML-escaped.
//
// Run with `npm test`. Same slice-the-real-source approach as
// ./board-forge-check.test.mjs: the RALPHUS-FORGE-RATE-LIMIT:BEGIN/END block in
// librarian/assets/board/67-review-inspector.js calls only `esc` and
// `fmtRelativeAge`, which the factory below supplies.

import test from "node:test";
import assert from "node:assert/strict";
import { boardScript } from "./board-source.mjs";

const BEGIN = "// RALPHUS-FORGE-RATE-LIMIT:BEGIN";
const END = "// RALPHUS-FORGE-RATE-LIMIT:END";

const html = boardScript();
const from = html.indexOf(BEGIN);
const to = html.indexOf(END);
if (from === -1 || to === -1 || to < from) {
  throw new Error(
    `forge-rate-limit: could not find the ${BEGIN} / ${END} markers in the board chunks. ` +
      "If this logic moved, move the markers with it -- these tests are its only coverage.",
  );
}
const source = html.slice(from + BEGIN.length, to);

const esc = (s) =>
  String(s).replace(/&/g, "&amp;").replace(/</g, "&lt;").replace(/>/g, "&gt;").replace(/"/g, "&quot;");
const fmtRelativeAge = (ms) => `${Math.round(ms / 1000)}s`;

// eslint-disable-next-line no-new-func -- evaluating the real shipped source is the point; see the header.
const { forgeRateLimitRowsHtml } = new Function(
  "esc",
  "fmtRelativeAge",
  `${source}
   return { forgeRateLimitRowsHtml };`,
)(esc, fmtRelativeAge);

const NOW = 1_700_000_000_000;

/** A healthy GitHub reply; tests override pieces. */
function status(overrides = {}) {
  return {
    now_ms: NOW,
    client_resolved: true,
    forge: "github",
    repo: "acme/widget",
    quota_tracked: true,
    quota: { limit: 5000, remaining: 4000, reset_at_ms: NOW + 600_000, observed_at_ms: NOW - 5_000 },
    backoff: { active: false, resume_at_ms: null, remaining_ms: null },
    checks: {
      ci_standing: { interval_ms: 120_000, next_due_at_ms: NOW + 30_000 },
      fast_watch_active: false,
      forge_cache: { enabled: true, interval_ms: 300_000 },
      next_attempt_at_ms: NOW + 30_000,
    },
    pull_requests: [],
    ...overrides,
  };
}

test("shows remaining calls, the limit and the reset", () => {
  const out = forgeRateLimitRowsHtml(status(), NOW);
  assert.match(out, /github · acme\/widget/);
  assert.match(out, /4000 of 5000 calls left/);
  assert.match(out, /resets in 600s/);
  assert.match(out, /not rate-limited/);
  assert.match(out, /next CI check in 30s/);
});

test("a nearly spent quota is flagged with the delayed colour", () => {
  const out = forgeRateLimitRowsHtml(
    status({ quota: { limit: 5000, remaining: 300, reset_at_ms: NOW + 60_000, observed_at_ms: NOW } }),
    NOW,
  );
  assert.match(out, /style="color:var\(--delayed\)"[^>]*>300 of 5000/);
  const healthy = forgeRateLimitRowsHtml(status(), NOW);
  assert.doesNotMatch(healthy, /--delayed/);
});

test("an active backoff shows the pause, its resume, and the pushed-back next check", () => {
  const out = forgeRateLimitRowsHtml(
    status({
      backoff: { active: true, resume_at_ms: NOW + 900_000, remaining_ms: 900_000 },
      checks: {
        ci_standing: { interval_ms: 120_000, next_due_at_ms: NOW + 30_000 },
        fast_watch_active: false,
        forge_cache: { enabled: true, interval_ms: 300_000 },
        next_attempt_at_ms: NOW + 900_000,
      },
    }),
    NOW,
  );
  assert.match(out, /paused: quota exhausted · resumes in 900s/);
  assert.match(out, /next CI check in 900s/);
  assert.match(out, /pushed back to the end of the pause above/);
  assert.doesNotMatch(out, /not rate-limited/);
});

test("an overdue next check reads 'due now' and an unscheduled one explains why", () => {
  const overdue = forgeRateLimitRowsHtml(
    status({ checks: { ...status().checks, next_attempt_at_ms: NOW - 1 } }),
    NOW,
  );
  assert.match(overdue, /next CI check: due now/);
  const never = forgeRateLimitRowsHtml(
    status({ checks: { ...status().checks, next_attempt_at_ms: null } }),
    NOW,
  );
  assert.match(never, /not scheduled yet/);
});

test("a branch with no PR and no reading still renders an explanation", () => {
  const out = forgeRateLimitRowsHtml(status({ quota: null, pull_requests: [] }), NOW);
  assert.match(out, /no quota reading yet/);
  assert.doesNotMatch(out, /#\d+/);
});

test("a forge without quota tracking says so, and an unresolved client is a failure row", () => {
  const gitlab = forgeRateLimitRowsHtml(status({ forge: "gitlab", quota: null, quota_tracked: false }), NOW);
  assert.match(gitlab, /quota readings are not available for this forge/);
  const none = forgeRateLimitRowsHtml(
    status({ client_resolved: false, forge: null, repo: null, quota: null, quota_tracked: false }),
    NOW,
  );
  assert.match(none, /style="color:var\(--failed\)"[^>]*>no forge client could be resolved/);
});

test("a PR with a failed poll shows its error in the failure colour; a clean PR is muted", () => {
  const out = forgeRateLimitRowsHtml(
    status({
      pull_requests: [
        {
          pr_id: "pr-1", pr_number: 616, state: "open", ci_checked_at_ms: NOW - 60_000,
          ci_check_error: "forge API 403: rate limited", drift_check_error: null, comments_check_error: null,
        },
        {
          pr_id: "pr-2", pr_number: 617, state: "open", ci_checked_at_ms: null,
          ci_check_error: null, drift_check_error: null, comments_check_error: null,
        },
      ],
    }),
    NOW,
  );
  assert.match(out, /style="color:var\(--failed\)"[^>]*>#616 \(open\): last CI check 60s ago · CI: forge API 403: rate limited/);
  assert.match(out, /style="color:var\(--muted\)"[^>]*>#617 \(open\): last CI check never/);
});

test("every row has a tooltip and daemon text is escaped", () => {
  const out = forgeRateLimitRowsHtml(
    status({
      repo: "a<b>/c",
      pull_requests: [
        {
          pr_id: "p", pr_number: 1, state: "open", ci_checked_at_ms: NOW,
          ci_check_error: "<script>x</script>", drift_check_error: null, comments_check_error: null,
        },
      ],
    }),
    NOW,
  );
  assert.doesNotMatch(out, /<script>/);
  assert.match(out, /a&lt;b&gt;\/c/);
  const rows = out.split('<div class="insp-pr-sub"').slice(1);
  assert.ok(rows.length >= 5);
  for (const r of rows) assert.match(r, /data-tip="/);
});
