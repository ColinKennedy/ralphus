# RAL-400 waypoints — Phase 5 decisions

Phase 5 is scenario 3, "in-flight delivery + parking" — what happens to a
squad's already-running cell when a waypoint it's rostered to (or surveyed
into) newly becomes `mode=block` while that cell is mid-flight. Per this
phase's own scope note, **only the hard-halt-on-block-gating mechanism is
pulled into v1**; the "graceful" half (turn-end checkpointing, live-turn
injection into a still-running backend, per-backend resume semantics) stays
deferred to v2. This file is the durable record of how the phase's two
open questions were resolved, so a later phase/ticket doesn't have to
re-derive them from code archaeology.

## Resumability question — resolved

**Parking mechanism: session-id retention + queue-slot release (built in
Phase 3) plus a ghost-fold note (built in this phase) that closes the
loop.** No new delivery channel, no new `CellSpec`/`RunnerSpec` field, no
new store table.

Concretely, when `daemon/src/scheduler.rs`'s cell worker sees
`result.is_waypoint_halted()`:

1. (Phase 3, already committed) `Store::mark_cell_waypoint_halted` records
   the halt; the cell's in-memory dispatch slot is released
   (`CellState::Detached`-shaped handling, mirroring a human-interactive
   detach) so the squad's worker thread can exit instead of polling
   forever; whatever `agent_session_id`/usage the runner captured live is
   persisted via `record_cell_result`. The periodic `run_pending_waypoint_
   resumes` sweep in `waypoints.rs` is what later hands the cell back to
   `pending` once the blocking waypoint closes or de-escalates — this part
   needed no new work in this phase.
2. (This phase, new) Before returning, the same branch now also fetches
   the blocking waypoint's current bearing list
   (`Store::list_waypoint_bearings`), renders it via the new
   `crate::waypoints::render_bearing_block`, and folds it into the cell's
   own ghost note via `Store::upsert_ghost` — the exact same handoff
   channel a cell's self-summarized `result.ghost` already publishes
   through a few lines below in the same file, and the exact same
   merge-not-overwrite semantics (`ghost.rs`'s `merge_content`), so a halt
   that follows an already-published self-summary augments it instead of
   clobbering it.
3. When the resume sweep later re-queues the cell, the scheduler's
   existing ghost-context prepend to a cell's `prompt` (already built for
   ordinary dependency handoffs, unrelated to waypoints) picks the folded
   note up automatically on redispatch — the agent sees the bearing
   context the same way it would see any other ghost note, with zero new
   code on the delivery side.

This satisfies the phase's own framing ("reusing `Store::upsert_ghost`/
`copy_ghost` for the ghost-fold note, built as part of what Phase 3 already
started, not separate v2 work") — the only genuinely new code was the
render step and the `upsert_ghost` call site, both wired into the halt
branch Phase 3 already built. `copy_ghost` itself needed no new call site:
nothing about a waypoint halt duplicates a ghost across owners, it only
appends to the halted cell's own.

Test: `daemon/src/scheduler.rs`'s
`a_waypoint_halted_cell_folds_current_bearings_into_its_own_ghost_note`
appends a bearing before halting a cell, then asserts the resulting ghost
note names the waypoint, carries the bearing's summary, and preserves its
commit reference.

## Explicit v2 deferrals

Recorded here per the phase's own AC ("record explicitly... that
advisory-mode in-flight delivery and full live-turn injection for
resumable backends remain deferred to v2, along with exactly-once/batching
semantics for multiple pending notes"):

- **Advisory-mode in-flight delivery.** An advisory-mode squad's in-flight
  cell is never halted (per Phase 0's already-settled advisory-mode
  meaning: "let it keep running while the note is delivered for
  awareness"), but v1 has no mechanism to actually push that note into a
  *running* cell — the ghost-fold path above only ever fires on the
  block-mode halt branch. An advisory bearing published while a cell is
  mid-flight is only picked up on that cell's *next* natural dispatch
  (finalize/dependent cell, or a later squad run), not delivered live.
  Building live delivery for the advisory case is v2 work.
- **Full live-turn injection for resumable backends.** Some backends
  support resuming an existing session (`codex exec resume`, `pi
  --session`) rather than only ever starting a fresh cell dispatch. v1's
  ghost-fold approach works uniformly across every backend precisely
  because it never tries to reach into a *live* turn — it waits for the
  next dispatch and relies on the existing prompt-prepend mechanism.
  Actually injecting a bearing mid-turn into a still-running resumable
  backend session (rather than waiting for its next dispatch) is
  unimplemented and deferred to v2. The unused-in-v1 `pending_injections`
  concept some earlier design notes reference is exactly this — it stays
  unimplemented.
- **Exactly-once/batching semantics for multiple pending notes.** If a
  waypoint accumulates several bearings (or a cell is halted, resumed,
  then halted again on a second waypoint) before a cell's next dispatch,
  v1's `render_bearing_block` simply re-renders and re-folds the *entire
  current* bearing list each time a halt occurs — `merge_content`'s
  dedup/merge behavior governs whether repeated folds compound, but there
  is no explicit exactly-once delivery tracking (no "already delivered
  bearing N" cursor) and no batching policy distinct from "whatever the
  list currently contains." Designing real exactly-once/batched delivery
  semantics for multiple pending notes is deferred to v2.

## What this phase did not touch

- The waypoint halt/resume mechanism itself (`mark_cell_waypoint_halted`,
  `clear_cell_waypoint_halted`, `resume_waypoint_halted_cell`,
  `squad_block_gating_waypoint`, `run_pending_waypoint_resumes`,
  `enqueue_waypoint_halt_mailbox`) — all Phase 3, unmodified here.
- `BearingView`'s shape — no new field was needed; the completed vs.
  requested vs. proposed distinction is inferred from whether
  `commit_id`/`commit_summary` are populated, exactly as Phase 2 already
  established, and `render_bearing_block` preserves that distinction in
  its rendered text rather than collapsing it.
