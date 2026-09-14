// Coverage for RAL-362's Tasks tab pure decision logic: usage aggregation,
// review/PR picking, watch resolution, the needs-me predicate, sort/filter/
// group helpers, and the virtualization window math. See
// ./board-task-tab-logic.mjs for how this is loaded out of the real board.html.
//
// Run with `npm test` (node --test).

import test from "node:test";
import assert from "node:assert/strict";
import { taskTabLogic } from "./board-task-tab-logic.mjs";

const {
  taskTabGridTemplate,
  ttUsageOf,
  ttCellUsageItems,
  ttTaskUsageItems,
  ttTaskUsage,
  ttFmtTokens,
  ttFmtCache,
  ttFmtTurns,
  ttFmtCost,
  ttTaskReviews,
  ttPickReviewBadge,
  ttPrsForTask,
  ttPickTaskPr,
  ttTaskEntityUri,
  ttSquadEntityUri,
  ttCellEntityUri,
  ttEffectiveWatch,
  ttEffectiveCellWatch,
  ttTierAllows,
  ttTaskNeedsMe,
  ttCompareRows,
  ttRowMatchesFilters,
  ttRowMatchesPrFilter,
  ttGroupAggregate,
  ttGroupAggregateWithGeneration,
  ttVisibleRange,
  ttBuildDisplayList,
} = taskTabLogic;

// ---------- usage aggregation ----------

test("ttUsageOf sums tokens/cache/cost across every item", () => {
  const u = ttUsageOf([
    { tokens_in: 10, tokens_out: 5, cache_creation_tokens: 2, cache_read_tokens: 1, cost_usd: 0.1 },
    { tokens_in: 3, tokens_out: 1, cost_usd: 0.2 },
  ]);
  assert.equal(u.tokensIn, 13);
  assert.equal(u.tokensOut, 6);
  assert.equal(u.cacheCreate, 2);
  assert.equal(u.cacheRead, 1);
  assert.ok(Math.abs(u.cost - 0.3) < 1e-9);
  assert.equal(u.anyCost, true);
  assert.equal(u.estimated, false);
});

test("ttUsageOf distinguishes no-cost-reported from a real zero cost", () => {
  const u = ttUsageOf([{ tokens_in: 5, tokens_out: 5 }]);
  assert.equal(u.anyCost, false);
  assert.equal(ttFmtCost(u), "–");
});

test("ttUsageOf marks estimated once any constituent is estimated", () => {
  const u = ttUsageOf([{ cost_usd: 0.5, cost_is_estimated: true }, { cost_usd: 0.5 }]);
  assert.equal(u.estimated, true);
  assert.equal(ttFmtCost(u), "~$1.00");
});

test("ttTaskUsageItems includes task-scope proof, cell usage, and cell-scope proof (but not double-counted)", () => {
  const task = {
    proof: [{ cost_usd: 1 }],
    cells: [
      { cost_usd: 2, proof: [{ cost_usd: 3 }] },
      { cost_usd: 4 },
    ],
  };
  const items = ttTaskUsageItems(task);
  const total = items.reduce((s, it) => s + (it.cost_usd || 0), 0);
  assert.ok(Math.abs(total - 10) < 1e-9);
  assert.equal(ttTaskUsage(task).cost, total);
});

test("ttCellUsageItems is just the cell plus its own proof steps", () => {
  const cell = { cost_usd: 1, proof: [{ cost_usd: 2 }, { cost_usd: 3 }] };
  assert.deepEqual(ttCellUsageItems(cell), [cell, ...cell.proof]);
});

test("ttFmtTokens/ttFmtCache render a dash when both figures are zero", () => {
  const zero = ttUsageOf([]);
  assert.equal(ttFmtTokens(zero), "–");
  assert.equal(ttFmtCache(zero), "–");
});

test("ttFmtTokens renders the in/out pair, ttFmtCache renders write/read", () => {
  const u = ttUsageOf([{ tokens_in: 10, tokens_out: 45, cache_creation_tokens: 7, cache_read_tokens: 3 }]);
  assert.equal(ttFmtTokens(u), "10 / 45");
  assert.equal(ttFmtCache(u), "7 / 3");
});

// ---------- review union + badge picking ----------

test("ttTaskReviews unions per-cell review refs into one entry per review id, collecting every distinct branch", () => {
  const task = {
    cells: [
      { reviews: [{ id: "g1", name: "R1", status: "in_review", branch: "b1", origin: "explicit" }] },
      { reviews: [{ id: "g1", name: "R1", status: "approved", branch: "b2", origin: "explicit" }] },
      { reviews: [{ id: "g2", name: "R2", status: "collecting", branch: "b1", origin: "arbiter" }] },
    ],
  };
  const reviews = ttTaskReviews(task);
  assert.equal(reviews.length, 2);
  const r1 = reviews.find((r) => r.id === "g1");
  assert.deepEqual(r1.branches, ["b1", "b2"]);
  // Last-seen cell wins for the union's status (mirrors the per-cell iteration order).
  assert.equal(r1.status, "approved");
});

test("ttPickReviewBadge picks the most attention-needing review and counts every review on the task", () => {
  const reviews = [
    { id: "g1", name: "R1", status: "approved", branches: ["b1"] },
    { id: "g2", name: "R2", status: "in_review", branches: ["b2"] },
  ];
  const badge = ttPickReviewBadge(reviews);
  assert.equal(badge.review.id, "g2");
  assert.equal(badge.count, 2);
});

test("ttPickReviewBadge returns null for a task on no reviews", () => {
  assert.equal(ttPickReviewBadge([]), null);
});

// ---------- PR picking ----------

test("ttPrsForTask matches on source_squad_id + source_task_idx only, ignoring source_cell_idx", () => {
  const idx = [
    { id: "pr1", source_squad_id: "s1", source_task_idx: 0, source_cell_idx: 0 },
    { id: "pr2", source_squad_id: "s1", source_task_idx: 0, source_cell_idx: 1 },
    { id: "pr3", source_squad_id: "s1", source_task_idx: 1, source_cell_idx: 0 },
    { id: "pr4", source_squad_id: "s2", source_task_idx: 0, source_cell_idx: 0 },
  ];
  const prs = ttPrsForTask(idx, "s1", 0);
  assert.deepEqual(prs.map((p) => p.id), ["pr1", "pr2"]);
});

test("ttPickTaskPr picks the earliest-created PR, tie-broken by lowest pr_number", () => {
  const prs = [
    { id: "pr1", created_at_ms: 200, pr_number: 5 },
    { id: "pr2", created_at_ms: 100, pr_number: 9 },
    { id: "pr3", created_at_ms: 100, pr_number: 3 },
  ];
  const pick = ttPickTaskPr(prs);
  assert.equal(pick.pr.id, "pr3");
  assert.equal(pick.count, 3);
});

test("ttPickTaskPr returns null for a task with no PRs", () => {
  assert.equal(ttPickTaskPr([]), null);
});

// ---------- watch resolution ----------

test("ttEffectiveWatch reports an explicit task watch as watched and not inherited", () => {
  const watches = [{ entity_uri: "task:s1:0" }];
  const w = ttEffectiveWatch(watches, new Set(), "s1", 0);
  assert.deepEqual(w, { watched: true, inherited: false });
});

test("ttEffectiveWatch reports a squad-level watch as watched and inherited", () => {
  const watches = [{ entity_uri: "squad:s1" }];
  const w = ttEffectiveWatch(watches, new Set(), "s1", 0);
  assert.deepEqual(w, { watched: true, inherited: true });
});

test("ttEffectiveWatch suppresses an inherited squad watch once the task is explicitly muted", () => {
  const watches = [{ entity_uri: "squad:s1" }];
  const muted = new Set([ttTaskEntityUri("s1", 0)]);
  const w = ttEffectiveWatch(watches, muted, "s1", 0);
  assert.deepEqual(w, { watched: false, inherited: false });
});

test("ttEffectiveWatch reports not watched when there's no explicit or squad-level watch", () => {
  const w = ttEffectiveWatch([], new Set(), "s1", 0);
  assert.deepEqual(w, { watched: false, inherited: false });
});

test("ttTaskEntityUri/ttSquadEntityUri match the daemon's EntityUri::Display grammar", () => {
  assert.equal(ttTaskEntityUri("s1", 2), "task:s1:2");
  assert.equal(ttSquadEntityUri("s1"), "squad:s1");
});

test("ttCellEntityUri matches the daemon's EntityUri::Display grammar", () => {
  assert.equal(ttCellEntityUri("s1", 2, 1), "cell:s1:2:1");
});

test("ttEffectiveCellWatch reports an explicit cell watch as watched and not inherited, even with no parent watch", () => {
  const watches = [{ entity_uri: "cell:s1:0:1" }];
  const w = ttEffectiveCellWatch(watches, new Set(), "s1", 0, 1);
  assert.deepEqual(w, { watched: true, inherited: false });
});

test("ttEffectiveCellWatch inherits from an explicit task watch", () => {
  const watches = [{ entity_uri: "task:s1:0" }];
  const w = ttEffectiveCellWatch(watches, new Set(), "s1", 0, 1);
  assert.deepEqual(w, { watched: true, inherited: true });
});

test("ttEffectiveCellWatch inherits from a squad watch two levels up", () => {
  const watches = [{ entity_uri: "squad:s1" }];
  const w = ttEffectiveCellWatch(watches, new Set(), "s1", 0, 1);
  assert.deepEqual(w, { watched: true, inherited: true });
});

test("ttEffectiveCellWatch is suppressed once the cell is explicitly muted, regardless of the parent watch source", () => {
  const watches = [{ entity_uri: "task:s1:0" }];
  const muted = new Set([ttCellEntityUri("s1", 0, 1)]);
  const w = ttEffectiveCellWatch(watches, muted, "s1", 0, 1);
  assert.deepEqual(w, { watched: false, inherited: false });
});

test("ttEffectiveCellWatch follows the task's own mute of a squad-level watch", () => {
  const watches = [{ entity_uri: "squad:s1" }];
  const muted = new Set([ttTaskEntityUri("s1", 0)]);
  const w = ttEffectiveCellWatch(watches, muted, "s1", 0, 1);
  assert.deepEqual(w, { watched: false, inherited: false });
});

test("ttEffectiveCellWatch reports not watched when nothing above it is watched", () => {
  const w = ttEffectiveCellWatch([], new Set(), "s1", 0, 1);
  assert.deepEqual(w, { watched: false, inherited: false });
});

// ---------- needs-me predicate ----------

test("ttTaskNeedsMe never fires for unwatched work, even if it failed", () => {
  const task = { state: "failed" };
  const r = ttTaskNeedsMe(task, { watched: false }, [], [], 0);
  assert.equal(r.needs, false);
});

test("ttTaskNeedsMe fires for a watched, failed task", () => {
  const task = { state: "failed" };
  const r = ttTaskNeedsMe(task, { watched: true }, [], [], 0);
  assert.equal(r.needs, true);
  assert.match(r.reason, /failed/);
});

test("ttTaskNeedsMe fires for a watched task with a review in_review", () => {
  const task = { state: "running" };
  const reviews = [{ name: "R1", status: "in_review" }];
  const r = ttTaskNeedsMe(task, { watched: true }, [], reviews, 0);
  assert.equal(r.needs, true);
  assert.match(r.reason, /in_review/);
});

test("ttTaskNeedsMe fires for an approved review with no PR, but not once a PR exists", () => {
  const task = { state: "running" };
  const reviews = [{ name: "R1", status: "approved" }];
  assert.equal(ttTaskNeedsMe(task, { watched: true }, [], reviews, 0).needs, true);
  assert.equal(ttTaskNeedsMe(task, { watched: true }, [], reviews, 1).needs, false);
});

test("ttTierAllows lets every trigger through when notify_tiers is empty (defaults to all tiers)", () => {
  assert.equal(ttTierAllows([], "failed"), true);
  assert.equal(ttTierAllows(undefined, "in_review"), true);
});

test("ttTierAllows gates a trigger on the watch's own notify tiers", () => {
  assert.equal(ttTierAllows(["urgent"], "failed"), true);
  assert.equal(ttTierAllows(["normal"], "failed"), false);
});

test("ttTaskNeedsMe respects a watch's notify tiers", () => {
  const task = { state: "failed" };
  const r = ttTaskNeedsMe(task, { watched: true }, ["normal"], [], 0);
  assert.equal(r.needs, false);
});

// ---------- sort / filter / group ----------

test("ttCompareRows sorts Tokens by the SUM of the displayed in/out pair", () => {
  const a = { usage: { tokensIn: 10, tokensOut: 45 } };
  const b = { usage: { tokensIn: 20, tokensOut: 20 } };
  assert.equal(ttCompareRows(a, b, "tokens"), 55 - 40);
});

test("ttCompareRows sorts Cache by the SUM of the write/read pair", () => {
  const a = { usage: { cacheCreate: 1, cacheRead: 1 } };
  const b = { usage: { cacheCreate: 10, cacheRead: 10 } };
  assert.ok(ttCompareRows(a, b, "cache") < 0);
});

// ---------- RAL-352 turns ----------

test("ttUsageOf sums turns only from constituents that carry the attribute (command-mode items contribute nothing)", () => {
  const u = ttUsageOf([{ turns: 4 }, { cost_usd: 0.5 }, { turns: 2 }, { tokens_in: 3 }]);
  assert.equal(u.turns, 6);
  assert.equal(u.anyTurns, true);
  assert.equal(ttFmtTurns(u), "6");
});

test("ttUsageOf reports no applicable turns when nothing carries the attribute", () => {
  const u = ttUsageOf([{ tokens_in: 1 }, { cost_usd: 0.5 }]);
  assert.equal(u.anyTurns, false);
  assert.equal(ttFmtTurns(u), "–");
});

test("ttUsageOf treats an explicit zero-turn agent row as zero, not as no-count", () => {
  const u = ttUsageOf([{ turns: 0 }, { tokens_in: 1 }]);
  assert.equal(u.anyTurns, true);
  assert.equal(ttFmtTurns(u), "0");
});

test("ttCompareRows sorts Turns numerically, treating no-count as zero", () => {
  const a = { usage: { turns: 0, anyTurns: false } };
  const b = { usage: { turns: 7, anyTurns: true } };
  assert.ok(ttCompareRows(a, b, "turns") < 0);
  assert.ok(ttCompareRows(b, b, "turns") === 0);
});

test("ttGroupAggregate sums turns and tracks anyTurns across the group's rows", () => {
  const rows = [
    { usage: { turns: 3, anyTurns: true } },
    { usage: { turns: 0, anyTurns: false } },
    { usage: { turns: 9, anyTurns: true } },
  ];
  const agg = ttGroupAggregate(rows);
  assert.equal(agg.turns, 12);
  assert.equal(agg.anyTurns, true);
});

test("ttCompareRows sorts squad by label, falling back to task index within the same squad", () => {
  const a = { squadLabel: "alpha", squadId: "s1", taskIdx: 1 };
  const b = { squadLabel: "alpha", squadId: "s1", taskIdx: 0 };
  assert.ok(ttCompareRows(a, b, "squad") > 0);
});

test("ttRowMatchesFilters applies the name filter, status set, project set, hidden-squad exclusion, needs-me, and PR filter together", () => {
  const row = { key: "s1:0", name: "Fix bug", state: "running", project: "alpha", squadId: "s1" };
  const filters = { q: "", status: new Set(["running"]), projects: new Set(), showHidden: false, needsMe: false, pr: false, prDraft: "any", prCi: "any" };
  assert.equal(ttRowMatchesFilters(row, filters, new Set(), new Set()), true);
  assert.equal(ttRowMatchesFilters(row, { ...filters, q: "nope" }, new Set(), new Set()), false);
  assert.equal(ttRowMatchesFilters(row, { ...filters, status: new Set(["done"]) }, new Set(), new Set()), false);
  // RAL-345: an empty projects set means "no filter" -- a row from any (or no) project shows; a non-empty set must contain the row's project.
  assert.equal(ttRowMatchesFilters(row, { ...filters, projects: new Set(["beta"]) }, new Set(), new Set()), false);
  assert.equal(ttRowMatchesFilters(row, { ...filters, projects: new Set(["alpha", "beta"]) }, new Set(), new Set()), true);
  assert.equal(ttRowMatchesFilters(row, { ...filters, projects: new Set() }, new Set(), new Set()), true);
  assert.equal(ttRowMatchesFilters(row, filters, new Set(["s1"]), new Set()), false);
  assert.equal(ttRowMatchesFilters(row, { ...filters, showHidden: true }, new Set(["s1"]), new Set()), true);
  assert.equal(ttRowMatchesFilters(row, { ...filters, needsMe: true }, new Set(), new Set()), false);
  assert.equal(ttRowMatchesFilters(row, { ...filters, needsMe: true }, new Set(), new Set(["s1:0"])), true);
  // RAL-345: an empty projects set means "no filter"; a non-empty one is an inclusion check.
  assert.equal(ttRowMatchesFilters(row, { ...filters, projects: new Set(["alpha"]) }, new Set(), new Set()), true);
  assert.equal(ttRowMatchesFilters(row, { ...filters, projects: new Set(["other"]) }, new Set(), new Set()), false);
});

// ---------- RAL-353 PR-state filter ----------

const pr = (/** @type {object} */ overrides) => ({ id: "pr-x", state: "open", draft: false, ci_status: null, ...overrides });
const prRow = (/** @type {object[]} */ prs) => ({ key: "s1:0", name: "T", state: "running", squadId: "s1", task: { project: "acme" }, prs });
const anyFilter = () => ({ pr: false, prDraft: "any", prCi: "any" });

test("ttRowMatchesPrFilter passes everything when no dimension is active", () => {
  assert.equal(ttRowMatchesPrFilter(prRow([]), anyFilter()), true);
  assert.equal(ttRowMatchesPrFilter(prRow([pr({})]), anyFilter()), true);
});

test("ttRowMatchesPrFilter with has-PR shows only tasks with at least one PR", () => {
  const f = { ...anyFilter(), pr: true };
  assert.equal(ttRowMatchesPrFilter(prRow([]), f), false);
  assert.equal(ttRowMatchesPrFilter(prRow([pr({ pr_number: 1 })]), f), true);
  assert.equal(ttRowMatchesPrFilter(prRow([pr({}), pr({})]), f), true);
});

test("ttRowMatchesPrFilter draft matches only PRs the forge reports as draft", () => {
  const f = { ...anyFilter(), prDraft: "draft" };
  assert.equal(ttRowMatchesPrFilter(prRow([pr({ draft: false })]), f), false);
  assert.equal(ttRowMatchesPrFilter(prRow([pr({ draft: true })]), f), true);
  // A PR whose draft state was never observed (null) is not known to be a draft.
  assert.equal(ttRowMatchesPrFilter(prRow([pr({ draft: null })]), f), false);
  // At least one matching PR suffices, even beside non-matching siblings.
  assert.equal(ttRowMatchesPrFilter(prRow([pr({ draft: false }), pr({ draft: true })]), f), true);
});

test("ttRowMatchesPrFilter non-draft matches false and never-observed (null), excluding drafts", () => {
  const f = { ...anyFilter(), prDraft: "non-draft" };
  assert.equal(ttRowMatchesPrFilter(prRow([pr({ draft: false })]), f), true);
  assert.equal(ttRowMatchesPrFilter(prRow([pr({ draft: null })]), f), true, "legacy-unknown counts as not-draft");
  assert.equal(ttRowMatchesPrFilter(prRow([pr({ draft: true })]), f), false);
});

test("ttRowMatchesPrFilter CI failing matches only the forge-reported failing status", () => {
  const f = { ...anyFilter(), prCi: "failing" };
  assert.equal(ttRowMatchesPrFilter(prRow([pr({ ci_status: "failing" })]), f), true);
  assert.equal(ttRowMatchesPrFilter(prRow([pr({ ci_status: "passing" })]), f), false);
  assert.equal(ttRowMatchesPrFilter(prRow([pr({ ci_status: "pending" })]), f), false, "pending is not failing");
  assert.equal(ttRowMatchesPrFilter(prRow([pr({ ci_status: null })]), f), false, "never-polled is not failing");
});

test("ttRowMatchesPrFilter CI passing matches only the forge-reported passing status", () => {
  const f = { ...anyFilter(), prCi: "passing" };
  assert.equal(ttRowMatchesPrFilter(prRow([pr({ ci_status: "passing" })]), f), true);
  assert.equal(ttRowMatchesPrFilter(prRow([pr({ ci_status: "failing" })]), f), false);
  assert.equal(ttRowMatchesPrFilter(prRow([pr({ ci_status: null })]), f), false);
});

test("ttRowMatchesPrFilter composes dimensions: draft + failing needs ONE PR that is both", () => {
  const f = { ...anyFilter(), prDraft: "draft", prCi: "failing" };
  assert.equal(ttRowMatchesPrFilter(prRow([pr({ draft: true, ci_status: "failing" })]), f), true);
  assert.equal(ttRowMatchesPrFilter(prRow([pr({ draft: true, ci_status: "passing" })]), f), false);
  assert.equal(ttRowMatchesPrFilter(prRow([pr({ draft: false, ci_status: "failing" })]), f), false);
  // A failing non-draft PR and a passing draft PR are two different PRs:
  // no single PR satisfies both dimensions, so the task does not match.
  assert.equal(
    ttRowMatchesPrFilter(prRow([pr({ draft: false, ci_status: "failing" }), pr({ draft: true, ci_status: "passing" })]), f),
    false,
  );
});

test("ttRowMatchesPrFilter has-PR combines with CI: any PR that is failing", () => {
  const f = { ...anyFilter(), pr: true, prCi: "failing" };
  assert.equal(ttRowMatchesPrFilter(prRow([pr({ ci_status: "passing" }), pr({ ci_status: "failing" })]), f), true);
  assert.equal(ttRowMatchesPrFilter(prRow([pr({ ci_status: "pending" })]), f), false);
});

test("ttRowMatchesFilters wires the PR-state filter into the row predicate", () => {
  const row = prRow([pr({ draft: true })]);
  const base = { q: "", status: new Set(["running"]), showHidden: false, needsMe: false, project: new Set() };
  assert.equal(ttRowMatchesFilters(row, { ...base, pr: true, prDraft: "any", prCi: "any" }, new Set(), new Set()), true);
  assert.equal(ttRowMatchesFilters(row, { ...base, pr: true, prDraft: "draft", prCi: "any" }, new Set(), new Set()), true);
  assert.equal(ttRowMatchesFilters(row, { ...base, pr: true, prDraft: "non-draft", prCi: "any" }, new Set(), new Set()), false);
});

test("ttGroupAggregate sums usage only across the rows passed in (post-filter scoping)", () => {
  const rows = [
    { usage: { tokensIn: 1, tokensOut: 1, cacheCreate: 0, cacheRead: 0, cost: 0.1, anyCost: true, estimated: false } },
    { usage: { tokensIn: 2, tokensOut: 2, cacheCreate: 0, cacheRead: 0, cost: 0, anyCost: false, estimated: true } },
  ];
  const agg = ttGroupAggregate(rows);
  assert.equal(agg.tokensIn, 3);
  assert.ok(Math.abs(agg.cost - 0.1) < 1e-9);
  assert.equal(agg.anyCost, true);
  assert.equal(agg.estimated, true);
});

test("ttGroupAggregateWithGeneration folds a squad's generation cost in exactly once and only once", () => {
  const rows = [
    { usage: { tokensIn: 10, tokensOut: 10, cacheCreate: 1, cacheRead: 2, cost: 0.4, anyCost: true, estimated: false, turns: 2, anyTurns: true } },
    { usage: { tokensIn: 1, tokensOut: 1, cacheCreate: 0, cacheRead: 0, cost: 0, anyCost: false, estimated: false, turns: 0, anyTurns: false } },
  ];
  const generation = { count: 3, tokens_in: 500, tokens_out: 120, cache_creation_tokens: 30, cache_read_tokens: 40, cost_usd: 0.05, estimated: false };
  // Nothing folds in when the squad reports no generation cost (the common case).
  assert.deepEqual(ttGroupAggregateWithGeneration(rows, undefined), ttGroupAggregate(rows));
  assert.deepEqual(
    ttGroupAggregateWithGeneration(rows, generation),
    { tokensIn: 511, tokensOut: 131, cacheCreate: 31, cacheRead: 42, cost: 0.45, anyCost: true, estimated: false, turns: 2, anyTurns: true }
  );
  // Estimation propagates to the group aggregate when any generation call's
  // figures are a live mid-run snapshot (RAL-326's flag, aggregated).
  const est = ttGroupAggregateWithGeneration(rows, { ...generation, estimated: true });
  assert.equal(est.estimated, true);
  assert.equal(est.cost, 0.45);
});

test("ttGroupAggregateWithGeneration honors anyCost semantics for a zero-reported generation cost", () => {
  const rows = [
    { usage: { tokensIn: 1, tokensOut: 1, cacheCreate: 0, cacheRead: 0, cost: 0, anyCost: false, estimated: false, turns: 0, anyTurns: false } },
  ];
  // A generation cost of exactly $0 (backend reported no dollar figure) must
  // not turn the squad's dash into a spurious "$0.00" -- same rule as ttUsageOf.
  const agg = ttGroupAggregateWithGeneration(rows, { count: 1, tokens_in: 7, tokens_out: 3, cache_creation_tokens: 0, cache_read_tokens: 0, cost_usd: 0, estimated: false });
  assert.equal(agg.anyCost, false);
  assert.equal(agg.cost, 0);
  assert.equal(agg.tokensIn, 8);
});

// ---------- virtualization ----------

test("ttVisibleRange computes the visible window with overscan on both sides", () => {
  const { first, last } = ttVisibleRange(320, 400, 32, 1000, 4);
  // scrollTop 320 / rowH 32 = row 10, minus 4 overscan = 6.
  assert.equal(first, 6);
  // viewportH 400 / rowH 32 = 12.5 -> ceil 13, + 2*4 overscan = 21 rows from `first`.
  assert.equal(last, 6 + 21);
});

test("ttVisibleRange clamps to 0 at the top and to totalRows at the bottom", () => {
  assert.equal(ttVisibleRange(0, 400, 32, 1000, 4).first, 0);
  assert.equal(ttVisibleRange(31900, 400, 32, 1000, 4).last, 1000);
});

test("ttBuildDisplayList flattens task rows without cell rows when nothing is expanded", () => {
  const rows = [{ key: "s1:0", squadId: "s1", cells: [{ id: "c1" }] }];
  const items = ttBuildDisplayList(rows, false, new Set());
  assert.equal(items.length, 1);
  assert.equal(items[0].type, "task");
});

test("ttBuildDisplayList inserts a task's cell rows immediately after it once expanded", () => {
  const rows = [{ key: "s1:0", squadId: "s1", cells: [{ id: "c1" }, { id: "c2" }] }];
  const items = ttBuildDisplayList(rows, false, new Set(["s1:0"]));
  assert.deepEqual(items.map((i) => i.type), ["task", "cell", "cell"]);
});

test("ttBuildDisplayList groups by squad with one group-header item per squad, preserving row order within a group", () => {
  const rows = [
    { key: "s1:0", squadId: "s1", cells: [] },
    { key: "s2:0", squadId: "s2", cells: [] },
    { key: "s1:1", squadId: "s1", cells: [] },
  ];
  const items = ttBuildDisplayList(rows, true, new Set());
  const groupSquadIds = items.filter((i) => i.type === "group").map((i) => i.squadId);
  assert.deepEqual(groupSquadIds, ["s1", "s2"]);
  const s1Group = items.find((i) => i.type === "group" && i.squadId === "s1");
  assert.equal(s1Group.rows.length, 2);
});

// ---------- column grid template ----------

test("taskTabGridTemplate omits hidden columns and applies per-column dragged widths", () => {
  const columns = [
    { key: "a", width: 100, min: 50, flex: false },
    { key: "b", width: 200, min: 50, flex: false },
    { key: "c", width: 300, min: 50, flex: true },
  ];
  const tmpl = taskTabGridTemplate(columns, new Set(["b"]), { a: 120 });
  assert.equal(tmpl, "120px minmax(50px, 1fr)");
});

test("taskTabGridTemplate never lets a dragged width go below the column's minimum", () => {
  const columns = [{ key: "a", width: 100, min: 80, flex: false }];
  const tmpl = taskTabGridTemplate(columns, new Set(), { a: 10 });
  assert.equal(tmpl, "80px");
});
