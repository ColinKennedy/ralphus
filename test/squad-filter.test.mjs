// Coverage for RAL-461: the Tasks sidebar's squad-filter predicate.
//
// A "go to cell" jump from the Reviews tab (or any other direct-navigation
// entry point) sets `revealedSquadId` to the target squad before switching
// to the Tasks tab. Before this fix, `revealedSquadId` only bypassed the
// per-user "hidden squad" preference (RAL-331) -- a squad excluded by the
// status checkboxes or the free-text query still vanished from the sidebar,
// so navigating to it could land you on a tab where the target squad looked
// unreachable. `squadMatchesTaskFilter` is the single predicate `visibleSquads`
// filters through; pinning it here locks in that `revealedSquadId` bypasses
// every filter, not just `hidden`, and that the bypass is scoped to exactly
// one squad at a time.
//
// Run with `npm test` (node --test). See ./board-squad-filter.mjs for how the
// predicate is loaded out of the real board.html.

import test from "node:test";
import assert from "node:assert/strict";
import { squadFilter, boardSource } from "./board-squad-filter.mjs";

const { squadMatchesTaskFilter } = squadFilter;

/** A minimal TaskFilters, overridable per test. */
function freshFilters(overrides = {}) {
  return {
    status: new Set(["pending", "running", "done"]),
    q: "",
    showHidden: false,
    ...overrides,
  };
}

test("a squad matching status/query/hidden passes with no reveal in play", () => {
  const r = { id: "squad-1", state: "running", label: "" };
  assert.equal(squadMatchesTaskFilter(r, freshFilters(), new Set(), null), true);
});

test("a squad whose status isn't checked is excluded when it isn't the revealed squad", () => {
  const r = { id: "squad-1", state: "failed", label: "" };
  const filters = freshFilters({ status: new Set(["running"]) });
  assert.equal(squadMatchesTaskFilter(r, filters, new Set(), null), false);
});

test("RAL-461: revealedSquadId bypasses the status filter", () => {
  const r = { id: "squad-1", state: "failed", label: "" };
  const filters = freshFilters({ status: new Set(["running"]) });
  assert.equal(squadMatchesTaskFilter(r, filters, new Set(), "squad-1"), true);
});

test("a squad not matching the text query is excluded when it isn't the revealed squad", () => {
  const r = { id: "squad-1", state: "running", label: "" };
  const filters = freshFilters({ q: "nomatch" });
  assert.equal(squadMatchesTaskFilter(r, filters, new Set(), null), false);
});

test("RAL-461: revealedSquadId bypasses the text-query filter", () => {
  const r = { id: "squad-1", state: "running", label: "" };
  const filters = freshFilters({ q: "nomatch" });
  assert.equal(squadMatchesTaskFilter(r, filters, new Set(), "squad-1"), true);
});

test("a hidden squad is excluded by default (RAL-331), unaffected by this fix", () => {
  const r = { id: "squad-1", state: "running", label: "" };
  assert.equal(squadMatchesTaskFilter(r, freshFilters(), new Set(["squad-1"]), null), false);
});

test("RAL-461: revealedSquadId still bypasses the hidden filter (RAL-331 behavior preserved)", () => {
  const r = { id: "squad-1", state: "running", label: "" };
  assert.equal(squadMatchesTaskFilter(r, freshFilters(), new Set(["squad-1"]), "squad-1"), true);
});

test("showHidden=true surfaces a hidden squad without needing a reveal", () => {
  const r = { id: "squad-1", state: "running", label: "" };
  const filters = freshFilters({ showHidden: true });
  assert.equal(squadMatchesTaskFilter(r, filters, new Set(["squad-1"]), null), true);
});

test("the reveal bypass is scoped to exactly the revealed squad, not every squad", () => {
  const revealed = { id: "squad-1", state: "failed", label: "" };
  const other = { id: "squad-2", state: "failed", label: "" };
  const filters = freshFilters({ status: new Set(["running"]) });
  assert.equal(squadMatchesTaskFilter(revealed, filters, new Set(), "squad-1"), true);
  assert.equal(squadMatchesTaskFilter(other, filters, new Set(), "squad-1"), false, "a squad that isn't the revealed one is filtered normally, even while a reveal is active");
});

test("once revealedSquadId moves off a squad, that squad is filtered normally again (single slot, not accumulated)", () => {
  const r = { id: "squad-1", state: "failed", label: "" };
  const filters = freshFilters({ status: new Set(["running"]) });
  assert.equal(squadMatchesTaskFilter(r, filters, new Set(), "squad-1"), true);
  assert.equal(squadMatchesTaskFilter(r, filters, new Set(), "squad-2"), false);
});

// Wiring: `visibleSquads` must filter through this predicate rather than
// re-implementing (or partially reimplementing) the filter logic inline —
// that inline duplication is exactly the bug this ticket fixed.

test("visibleSquads filters through squadMatchesTaskFilter", () => {
  const body = boardSource.slice(boardSource.indexOf("function visibleSquads()"));
  const fn = body.slice(0, body.indexOf("\n      }\n") + 1);
  assert.match(fn, /squadMatchesTaskFilter\(r, filters, hiddenSquadIds, revealedSquadId\)/);
});
