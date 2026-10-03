// Coverage for RAL-537 "surface auto-fix / base-shift rebuild exhaustion":
//
// - `autoFixExhaustedBadge` renders a per-worktree-chip badge when an open PR
//   on that branch has exhausted its unattended CI auto-fix budget, listing
//   both PRs in the tooltip when the branch's parent and stack PRs are both
//   exhausted (dual_root_pr mode), and staying silent otherwise (no PR on the
//   branch, the PR isn't open, or it hasn't exhausted its budget);
// - `rebaseExhaustedNotice` renders a single review-level notice (not a
//   per-branch badge, since the underlying counter is shared across the whole
//   review -- RAL-542 tracks making it per-branch) when the base-shift
//   rebuild campaign has used up its attempt budget, gated on the same
//   terminal-status `canReorder` check the caller already computes.
//
// Run with `npm test` (node --test). See ./board-exhausted-badges.mjs for how
// the regions are loaded out of the real board chunks.

import test from "node:test";
import assert from "node:assert/strict";
import { makeExhaustedBadges } from "./board-exhausted-badges.mjs";

// ---------- autoFixExhaustedBadge ----------

test("no badge when no PR is attached to the branch", () => {
  const { autoFixExhaustedBadge } = makeExhaustedBadges();
  const html = autoFixExhaustedBadge([], { id: "branch-1" });
  assert.equal(html, "");
});

test("no badge when the exhausted PR belongs to a different branch", () => {
  const { autoFixExhaustedBadge } = makeExhaustedBadges();
  const prs = [{ branch_id: "other-branch", state: "open", auto_fix_last_outcome: "exhausted", pr_number: 7, auto_fix_attempt_count: 3 }];
  const html = autoFixExhaustedBadge(prs, { id: "branch-1" });
  assert.equal(html, "");
});

test("no badge when the exhausted PR is no longer open", () => {
  const { autoFixExhaustedBadge } = makeExhaustedBadges();
  const prs = [{ branch_id: "branch-1", state: "merged", auto_fix_last_outcome: "exhausted", pr_number: 7, auto_fix_attempt_count: 3 }];
  const html = autoFixExhaustedBadge(prs, { id: "branch-1" });
  assert.equal(html, "");
});

test("no badge when the open PR has not exhausted its auto-fix budget", () => {
  const { autoFixExhaustedBadge } = makeExhaustedBadges();
  const prs = [{ branch_id: "branch-1", state: "open", auto_fix_last_outcome: null, pr_number: 7, auto_fix_attempt_count: 1 }];
  const html = autoFixExhaustedBadge(prs, { id: "branch-1" });
  assert.equal(html, "");
});

test("badge appears for a single exhausted open PR, using --muted and a data-tip", () => {
  const { autoFixExhaustedBadge } = makeExhaustedBadges();
  const prs = [{ branch_id: "branch-1", state: "open", auto_fix_last_outcome: "exhausted", pr_number: 42, auto_fix_attempt_count: 3 }];
  const html = autoFixExhaustedBadge(prs, { id: "branch-1" });
  assert.match(html, />⚠ auto-fix exhausted</);
  assert.match(html, /color:var\(--muted\)/);
  assert.doesNotMatch(html, /#[0-9a-fA-F]{3,6}/, "no hardcoded hex color");
  assert.match(html, /data-tip="[^"]*PR #42[^"]*"/);
  assert.match(html, /data-tip="[^"]*tried 3 time\(s\)[^"]*"/);
  assert.match(html, /data-tip="[^"]*Merge \/ rebase[^"]*"/);
});

test("badge tooltip names both PRs when the parent and stack PR are both exhausted", () => {
  const { autoFixExhaustedBadge } = makeExhaustedBadges();
  const prs = [
    { branch_id: "branch-1", state: "open", auto_fix_last_outcome: "exhausted", pr_number: 1, auto_fix_attempt_count: 2, pr_kind: "parent" },
    { branch_id: "branch-1", state: "open", auto_fix_last_outcome: "exhausted", pr_number: 2, auto_fix_attempt_count: 5, pr_kind: "stack" },
  ];
  const html = autoFixExhaustedBadge(prs, { id: "branch-1" });
  const tipMatch = html.match(/data-tip="([^"]*)"/);
  assert.ok(tipMatch, "expected a data-tip attribute");
  assert.match(tipMatch[1], /PR #1/);
  assert.match(tipMatch[1], /PR #2/);
});

test("badge tooltip falls back to '?' when pr_number is missing", () => {
  const { autoFixExhaustedBadge } = makeExhaustedBadges();
  const prs = [{ branch_id: "branch-1", state: "open", auto_fix_last_outcome: "exhausted", pr_number: null, auto_fix_attempt_count: 1 }];
  const html = autoFixExhaustedBadge(prs, { id: "branch-1" });
  assert.match(html, /data-tip="[^"]*PR #\?[^"]*"/);
});

test("the badge tooltip is escaped through the injected esc", () => {
  const calls = [];
  const { autoFixExhaustedBadge } = makeExhaustedBadges({
    esc: (s) => { calls.push(s); return `ESCAPED(${s})`; },
  });
  const prs = [{ branch_id: "branch-1", state: "open", auto_fix_last_outcome: "exhausted", pr_number: 3, auto_fix_attempt_count: 1 }];
  const html = autoFixExhaustedBadge(prs, { id: "branch-1" });
  assert.equal(calls.length, 1);
  assert.match(html, /data-tip="ESCAPED\(/);
});

// ---------- rebaseExhaustedNotice ----------

test("no notice when the review is in a terminal status (canReorder false)", () => {
  const { rebaseExhaustedNotice } = makeExhaustedBadges();
  const g = { base_shift_rebuild_attempts: 5, effective_base_shift_maximum_rebuilds: 3 };
  assert.equal(rebaseExhaustedNotice(g, false), "");
});

test("no notice when attempts are below the cap", () => {
  const { rebaseExhaustedNotice } = makeExhaustedBadges();
  const g = { base_shift_rebuild_attempts: 1, effective_base_shift_maximum_rebuilds: 3 };
  assert.equal(rebaseExhaustedNotice(g, true), "");
});

test("no notice when there is no effective cap configured", () => {
  const { rebaseExhaustedNotice } = makeExhaustedBadges();
  const g = { base_shift_rebuild_attempts: 5, effective_base_shift_maximum_rebuilds: 0 };
  assert.equal(rebaseExhaustedNotice(g, true), "");
});

test("notice appears once attempts reach the cap, using --muted and a data-tip", () => {
  const { rebaseExhaustedNotice } = makeExhaustedBadges();
  const g = { base_shift_rebuild_attempts: 3, effective_base_shift_maximum_rebuilds: 3 };
  const html = rebaseExhaustedNotice(g, true);
  assert.match(html, /color:var\(--muted\)/);
  assert.doesNotMatch(html, /#[0-9a-fA-F]{3,6}/, "no hardcoded hex color");
  assert.match(html, />⚠ Automatic rebasing stopped after 3\/3 failed attempt\(s\)/);
  assert.match(html, /data-tip="[^"]*cap: 3[^"]*"/);
  assert.match(html, /data-tip="[^"]*Merge \/ rebase[^"]*"/);
});

test("notice remains once attempts exceed the cap", () => {
  const { rebaseExhaustedNotice } = makeExhaustedBadges();
  const g = { base_shift_rebuild_attempts: 4, effective_base_shift_maximum_rebuilds: 3 };
  const html = rebaseExhaustedNotice(g, true);
  assert.match(html, />⚠ Automatic rebasing stopped after 4\/3 failed attempt\(s\)/);
});

test("per-worktree map: notice names only the exhausted worktree and ignores the pass counter", () => {
  const { rebaseExhaustedNotice } = makeExhaustedBadges();
  const g = {
    base_shift_rebuild_attempts: 3,
    effective_base_shift_maximum_rebuilds: 3,
    base_shift_rebuild_attempts_by_project: { "/repo/alpha": 3, "/repo/beta": 1 },
  };
  const html = rebaseExhaustedNotice(g, true);
  assert.match(html, />⚠ Automatic rebasing stopped for alpha after 3\/3 failed attempt\(s\)/);
  assert.doesNotMatch(html, /beta/);
  assert.match(html, /data-tip="[^"]*other worktree[^"]*keeps rebasing[^"]*"/);
});

test("per-worktree map: no notice while every worktree still has budget, even if the pass counter hit the cap", () => {
  const { rebaseExhaustedNotice } = makeExhaustedBadges();
  const g = {
    base_shift_rebuild_attempts: 4,
    effective_base_shift_maximum_rebuilds: 3,
    base_shift_rebuild_attempts_by_project: { "/repo/alpha": 2, "/repo/beta": 1 },
  };
  assert.equal(rebaseExhaustedNotice(g, true), "");
});

test("per-worktree map: Windows-style roots show their last path segment", () => {
  const { rebaseExhaustedNotice } = makeExhaustedBadges();
  const g = {
    effective_base_shift_maximum_rebuilds: 2,
    base_shift_rebuild_attempts_by_project: { "C:\\repo\\alpha": 2 },
  };
  assert.match(rebaseExhaustedNotice(g, true), /stopped for alpha after 2\/2/);
});
