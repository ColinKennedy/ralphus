// RAL-419: automated coverage for the Squads tab's per-squad selection cache —
// the restore-vs-explicit transition and stale-selection reconciliation that
// let the board bring back a squad's exact primary + multi selection when the
// user returns to it, across squad switches and browser refreshes.
//
// The marker-extracted region (see ./board-sel-transition.mjs) is pure: it
// takes selection state and a squad graph in, and returns what the board should
// show — no DOM, no fetch, no localStorage. The tests below pin down the
// behaviors the ticket's acceptance criteria name:
//
//   - selecting a cell/task/proof (or the squad banner), switching squads, and
//     returning restores that same entity (transitionSquadSelection);
//   - an explicit click on the squad already on screen selects the banner and
//     never revives a child selection;
//   - a poll/SSE refresh that removes the selected node drops the selection to
//     the nearest surviving parent (proof -> cell -> task -> squad) and flags
//     the entry stale so the caller clears it (reconcileSquadSelection);
//   - persisted node-multi-selection keys are filtered to keys that still name
//     live graph nodes (nodeKeyValid);
//   - caches for deleted squads are pruned against the loaded squad list
//     (pruneSquadSelCache).
//
// Run with `npm test` (node --test). See ./board-sel-transition.mjs for how the
// region is loaded out of the real board chunks.
//
// ---- Browser-manual coverage (RAL-419) ----
//
// The pure tests above cover the decision logic; the rendered outcome (banner
// highlight, details-pane content, localStorage survival across a hard refresh)
// is browser-only and is exercised by hand:
//
//   1. `bash scripts/build-debug.sh`, open http://127.0.0.1:7474, submit a
//      squad with a couple of tasks/cells (or reuse an existing one).
//   2. Cell -> other squad -> return: click a cell, click a *different* squad
//      in the sidebar, click the original squad again. Expect: the cell is the
//      selected graph node again — the squad banner is NOT highlighted and the
//      details pane shows the cell's information (not the squad view).
//   3. Squad -> other squad -> return: click the original squad's banner (or
//      click the squad you're already on, which selects its banner), switch to
//      another squad, return. Expect: the squad banner is highlighted and the
//      squad details show.
//   4. Explicit click: with a cell selected, click the *same* squad in the
//      sidebar. Expect: banner selection, never the previous cell.
//   5. Hard refresh (Ctrl-F5) while a cell is selected; expect the same squad
//      (the last focused one) to come back with the same cell selected and the
//      graph multi-highlight intact. Restart the daemon/librarian too — the
//      cache lives in localStorage, not in the server.
//   6. Select a cell, then restart the squad from its menu; after the
//      refresh removes/re-creates the cell, expect the selection to rest on the
//      nearest surviving parent rather than an index that points at nothing.
//   7. Shift/Ctrl-click several nodes, switch squads, return: expect the same
//      multi-selection restored, and a plain bulk-squad multi-select (ctrl-click
//      rows without graph nodes) to stay unaffected by all of the above.

import test from "node:test";
import assert from "node:assert/strict";
import { selTransition } from "./board-sel-transition.mjs";

const { squadLevelSel, snapshotSel, reconcileSquadSelection, nodeKeyValid, transitionSquadSelection, pruneSquadSelCache } = selTransition;

/** A squad graph with two tasks; task 0 has two cells, cell-scope proofs; task 1 has a task-scope proof only. */
const SQUAD = {
  id: "s-1",
  label: "one",
  state: "running",
  created_at_ms: 1,
  tasks: [
    {
      name: "t0", project: "p", agent: null, model: null, state: "done", soloed: false,
      cells: [
        { id: "c0", cwd: ".", agent: "x", model: null, state: "done", proof: [{ kind: "command", id: "p0", state: "done", output: null, spec: "" }] },
        { id: "c1", cwd: ".", agent: "x", model: null, state: "done", proof: [] },
      ],
      proof: [{ kind: "command", id: "tp0", state: "done", output: null, spec: "" }],
    },
    { name: "t1", project: "p", agent: null, model: null, state: "pending", soloed: false, cells: [], proof: [] },
  ],
};

/** A plain cell selection — the ordinary single-item case that must restore too. */
const CELL = { kind: "cell", taskIdx: 0, cellIdx: 1, proofIdx: -1 };
const TASK = { kind: "task", taskIdx: 1, cellIdx: -1, proofIdx: -1 };
const PROOF_TASK = { kind: "proof", taskIdx: 0, cellIdx: -1, proofIdx: 0 };
const PROOF_CELL = { kind: "proof", taskIdx: 0, cellIdx: 0, proofIdx: 0 };
const BANNER = { kind: "squad", taskIdx: 0, cellIdx: 0, proofIdx: -1 };

// ---------- snapshot ----------

test("snapshotSel copies the full selection shape and normalizes a missing proof index", () => {
  assert.deepEqual(snapshotSel({ kind: "squad", taskIdx: 0, cellIdx: 0 }), BANNER, "missing proofIdx becomes -1");
  assert.deepEqual(snapshotSel(CELL), CELL, "a full shape passes through unchanged");
  assert.deepEqual(snapshotSel({ kind: null, taskIdx: 0, cellIdx: 0, proofIdx: -1 }), { kind: null, taskIdx: 0, cellIdx: 0, proofIdx: -1 });
});

// ---------- reconciliation: nearest surviving parent ----------

test("a valid cell selection is untouched and not stale", () => {
  const rec = reconcileSquadSelection(SQUAD, CELL);
  assert.equal(rec.stale, false);
  assert.deepEqual(rec.sel, CELL);
});

test("a valid task selection is untouched and not stale", () => {
  assert.equal(reconcileSquadSelection(SQUAD, TASK).stale, false);
});

test("a valid proof selection (task-scope and cell-scope) is untouched and not stale", () => {
  assert.equal(reconcileSquadSelection(SQUAD, PROOF_TASK).stale, false);
  assert.equal(reconcileSquadSelection(SQUAD, PROOF_CELL).stale, false);
});

test("a banner selection is always valid — never stale", () => {
  const rec = reconcileSquadSelection(SQUAD, BANNER);
  assert.equal(rec.stale, false);
  assert.deepEqual(rec.sel, BANNER);
});

test("a cell-scope proof whose proof step vanished falls back to its cell (stale)", () => {
  const rec = reconcileSquadSelection(SQUAD, { kind: "proof", taskIdx: 0, cellIdx: 0, proofIdx: 5 });
  assert.equal(rec.stale, true);
  assert.deepEqual(rec.sel, { kind: "cell", taskIdx: 0, cellIdx: 0, proofIdx: -1 });
});

test("a task-scope proof whose proof step vanished falls back to its task (stale)", () => {
  const rec = reconcileSquadSelection(SQUAD, { kind: "proof", taskIdx: 0, cellIdx: -1, proofIdx: 9 });
  assert.equal(rec.stale, true);
  assert.deepEqual(rec.sel, { kind: "task", taskIdx: 0, cellIdx: -1, proofIdx: -1 });
});

test("a cell whose index is gone falls back to its task (stale)", () => {
  const rec = reconcileSquadSelection(SQUAD, { kind: "cell", taskIdx: 0, cellIdx: 7, proofIdx: -1 });
  assert.equal(rec.stale, true);
  assert.deepEqual(rec.sel, { kind: "task", taskIdx: 0, cellIdx: -1, proofIdx: -1 });
});

test("a task whose index is gone falls back to the squad banner (stale)", () => {
  const rec = reconcileSquadSelection(SQUAD, { kind: "task", taskIdx: 9, cellIdx: -1, proofIdx: -1 });
  assert.equal(rec.stale, true);
  assert.deepEqual(rec.sel, BANNER);
});

test("a missing squad makes any child selection stale; the banner itself stays valid", () => {
  assert.equal(reconcileSquadSelection(undefined, CELL).stale, true);
  assert.equal(reconcileSquadSelection(undefined, CELL).sel.kind, "squad");
  assert.equal(reconcileSquadSelection(undefined, BANNER).stale, false);
});

test("reconciliation is deterministic and ends at the squad banner at worst", () => {
  // proof -> missing cell -> missing task: the walk terminates at the banner.
  const rec = reconcileSquadSelection(SQUAD, { kind: "proof", taskIdx: 9, cellIdx: 3, proofIdx: 0 });
  assert.equal(rec.stale, true);
  assert.deepEqual(rec.sel, BANNER);
});

// ---------- transition: restoring vs explicit selection ----------

test("focusing the squad already on screen is an explicit click: banner, no restore", () => {
  const t = transitionSquadSelection({ "s-1": CELL }, { "s-1": ["cell:0:1:-1"] }, "s-1", SQUAD, "s-1");
  assert.deepEqual(t.sel, BANNER);
  assert.deepEqual(t.nodeKeys, []);
  assert.equal(t.stale, false);
});

test("focusing a different squad with a cached child selection restores it (return path)", () => {
  const t = transitionSquadSelection({ "s-1": CELL }, { "s-1": ["cell:0:1:-1", "task:1:-1:-1"] }, "s-1", SQUAD, "s-9");
  assert.deepEqual(t.sel, CELL, "the exact primary selection returns - this is AC's cell -> other -> return");
  assert.deepEqual(t.nodeKeys, ["cell:0:1:-1", "task:1:-1:-1"], "multi-selection rides along");
  assert.equal(t.stale, false);
});

test("focusing a different squad whose cache holds a banner selection restores the banner, not a child", () => {
  const t = transitionSquadSelection({ "s-1": BANNER }, { "s-1": [] }, "s-1", SQUAD, "s-9");
  assert.deepEqual(t.sel, BANNER, "a squad banner is restored only when it was the actual prior selection");
});

test("focusing a squad with no cache entry selects the banner (first visit)", () => {
  const t = transitionSquadSelection({}, {}, "s-1", SQUAD, "s-9");
  assert.deepEqual(t.sel, BANNER);
  assert.deepEqual(t.nodeKeys, []);
});

test("cached multi-selection keys that no longer name live nodes are filtered out", () => {
  const t = transitionSquadSelection(
    { "s-1": CELL },
    { "s-1": ["cell:0:1:-1", "cell:0:7:-1", "task:9:-1:-1", "nonsense"] },
    "s-1", SQUAD, "s-9",
  );
  assert.deepEqual(t.nodeKeys, ["cell:0:1:-1"], "only the live cell key survives the restore");
  assert.deepEqual(t.sel, CELL, "the primary selection survives even when some multi keys were pruned");
});

test("a stale cached child selection surfaces as stale with the nearest parent, and no node keys", () => {
  const t = transitionSquadSelection({ "s-1": { kind: "cell", taskIdx: 0, cellIdx: 7, proofIdx: -1 } }, { "s-1": ["cell:0:7:-1"] }, "s-1", SQUAD, "s-9");
  assert.equal(t.stale, true, "the caller must clear the stale cache entry");
  assert.deepEqual(t.sel, { kind: "task", taskIdx: 0, cellIdx: -1, proofIdx: -1 }, "the nearest surviving parent is what displays");
  assert.deepEqual(t.nodeKeys, [], "no dead keys ride along");
});

// ---------- node keys ----------

test("nodeKeyValid accepts live task/cell/proof keys and rejects dead ones", () => {
  assert.equal(nodeKeyValid(SQUAD, "task:1:-1:-1"), true);
  assert.equal(nodeKeyValid(SQUAD, "task:9:-1:-1"), false);
  assert.equal(nodeKeyValid(SQUAD, "cell:0:1:-1"), true);
  assert.equal(nodeKeyValid(SQUAD, "cell:0:7:-1"), false);
  assert.equal(nodeKeyValid(SQUAD, "proof:0:-1:0"), true, "task-scope proof key");
  assert.equal(nodeKeyValid(SQUAD, "proof:0:0:0"), true, "cell-scope proof key");
  assert.equal(nodeKeyValid(SQUAD, "proof:0:0:9"), false);
  assert.equal(nodeKeyValid(SQUAD, "proof:0:7:0"), false);
});

test("nodeKeyValid rejects malformed keys and non-squad inputs", () => {
  for (const bad of ["", "cell:0:1", "cell:0:1:-1:extra", "cell:0:1.5:-1", "banana:0:1:-1", "cell:-1:0:-1", 42, null, undefined]) {
    assert.equal(nodeKeyValid(SQUAD, bad), false, `key ${JSON.stringify(bad)} must be rejected`);
  }
  assert.equal(nodeKeyValid(undefined, "task:1:-1:-1"), false);
});

// ---------- prune ----------

test("pruneSquadSelCache drops only squads absent from the alive list", () => {
  const cache = { a: BANNER, b: CELL, c: { kind: "task", taskIdx: 0, cellIdx: -1, proofIdx: -1 } };
  const nodeCache = { a: [], b: ["cell:0:0:-1"], c: [] };
  pruneSquadSelCache(cache, nodeCache, ["a", "b"]);
  assert.deepEqual(Object.keys(cache).sort(), ["a", "b"]);
  assert.deepEqual(Object.keys(nodeCache).sort(), ["a", "b"]);
  assert.deepEqual(cache.a, BANNER, "survivors are untouched");
});