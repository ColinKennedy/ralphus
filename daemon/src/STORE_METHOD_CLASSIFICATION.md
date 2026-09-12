# RAL-393 Stage 3 -- `Store` method read/write classification

This table classifies every `pub fn` / `pub(crate) fn` directly inside the
two `impl Store` blocks in `daemon/src/store.rs` (lines 821-5239 and
5240-8562) into one of four categories, per the RAL-393 ticket's Stage 3
requirement ("classify all ~161 `pub fn`/`pub(crate) fn` methods on `Store`
as read or write"). Methods on other types defined in this file (the
`SquadState`/`NodeState` enums' `as_str`/`parse`/`is_terminal`/
`satisfies_dependents`) and free functions (`now_ms`, `to_json`,
`from_json`, `to_json_map`, `from_json_map`) are not `Store` methods and are
excluded.

- **`read`** -- only ever executes read-only SQL (`query_row`, `query_map`,
  `prepare` + a `SELECT`) against `self.conn`, touches none of `Store`'s
  in-memory fields, and calls no `write`-classified method. Safe to run
  against a [`crate::store_pool::ReadConnPool`] pooled connection instead of
  [`crate::store_lock::StoreMutex`] -- see [`Store::running_count_conn`] for
  the reference pattern this migration uses.
- **`write`** -- executes any `INSERT`/`UPDATE`/`DELETE`/DDL against
  `self.conn` (directly, via `.transaction()`, or by calling another
  `write`-classified method), regardless of whether it also reads first.
  Must stay serialized behind `StoreMutex` and the single writer connection.
- **`memory`** -- touches one of `Store`'s in-memory fields
  (`live_activity`, `guardian_summary_debounce`, `guardian_worktree_leases`,
  `guardian_restack_requests`, `guardian_restack_running`,
  `stall_escalated`, `secret_env_names_cache`) and little or no SQL. Split
  into `memory (read)` (reads the field only) and `memory (write)` (mutates
  it) -- neither is poolable via `ReadConnPool`, since that pool has no
  access to `Store`'s fields at all; both must stay behind `StoreMutex`, the
  same as any other in-process shared mutable state.
- **`mixed`** -- a pure delegator with no direct SQL of its own that calls a
  genuine mix of `read`- and `write`-classified sub-methods, where whether a
  write actually happens depends on a runtime condition (e.g. `activate`
  only calls the write path when the squad isn't already running). Must
  stay serialized like `write`, since the write branch can execute.

Composite methods that unconditionally call both a read and a write helper
(e.g. `restart_squad`, which always resets and always logs) are classified
`write`, not `mixed` -- see the per-row notes for the reasoning on each
boundary case.

## Table

Sorted by line number. `Note` gives the direct SQL/field evidence, or the
transitive callee(s) relied on for a delegating method.

| line | method | category | note |
|---|---|---|---|
| 828 | try_acquire_guardian_worktree_lease | memory (write) | reads `guardian_restack_running`, mutates `guardian_worktree_leases` |
| 849 | release_guardian_worktree_lease | memory (write) | conditionally removes from `guardian_worktree_leases` |
| 866 | guardian_worktree_lease_owner | memory (read) | reads `guardian_worktree_leases` |
| 880 | request_guardian_restack | memory (write) | mutates `guardian_restack_requests` |
| 894 | try_claim_guardian_restack | memory (write) | reads 2 fields, mutates `guardian_restack_requests` + `guardian_restack_running` |
| 911 | finish_guardian_restack | memory (write) | removes from `guardian_restack_running` |
| 916 | open | write | opens conn, runs `init_schema` DDL/migrations; constructs all 7 in-memory fields |
| 954 | open_in_memory | write | same as `open` but on a shared-cache in-memory DB |
| 988 | read_pool | read | trivial `Arc::clone` accessor, touches neither `self.conn` nor memory fields |
| 995 | event_bus | read | trivial accessor, touches neither `self.conn` nor memory fields |
| 2617 | next_id | write | UPSERT increments a `meta` sequence row, then selects it |
| 2637 | insert_squad | write | delegates to `next_id` (write) then `insert_squad_with_id` (write) |
| 2664 | insert_squad_with_id | write | transaction: INSERT squads/tasks/cells/proofs + `cartographer_log` |
| 2845 | log_event | write | delegates to `log_event_with_task` (INSERT events) |
| 2860 | log_event_with_task | write | INSERT into `events`; calls `cartographer_log` (write) |
| 2894 | events_for_squad | read | SELECT via `events_where` helper |
| 2899 | events_for_guardian | read | SELECT via `events_where` helper |
| 2926 | squad_state | read | `SELECT state FROM squads` |
| 2942 | set_squad_state | write | direct `UPDATE squads`; also calls `log_event` |
| 2987 | squad_trace_context | read | `SELECT trace_context` |
| 3002 | set_squad_trace_context | write | `UPDATE squads SET trace_context` |
| 3015 | activate | mixed | reads `squad_state` then conditionally calls `set_squad_state` (write) |
| 3036 | cancel | mixed | reads `squad_state` then conditionally calls `set_squad_state`/`cancel_unfinished_nodes` (write) |
| 3068 | list_ready | read | SELECT pending squads, then `deps_satisfied` (SELECT only) |
| 3107 | running_count | read | delegates to `running_count_conn` |
| 3115 | running_count_conn | read | `SELECT COUNT(*) FROM squads` -- reference pattern already pooled (see `Daemon::read_running_count`) |
| 3126 | running_cell_count | read | `SELECT COUNT(*) FROM cells` |
| 3136 | merging_guardians | read | `SELECT id,name FROM guardians` |
| 3149 | set_cell_state | write | direct `UPDATE cells`; also calls `log_event_with_task` |
| 3213 | set_task_state | write | direct `UPDATE tasks`; also calls `log_event_with_task` |
| 3272 | set_task_error | write | `UPDATE tasks SET error` |
| 3293 | solo_task | write | delegates to `set_task_soloed(...,true)` (UPDATE + log_event) |
| 3302 | unsolo_task | write | delegates to `set_task_soloed(...,false)` |
| 3337 | soloed_task_indices | read | `SELECT idx FROM tasks WHERE soloed=1` |
| 3352 | cell_state | read | `SELECT state FROM cells` |
| 3375 | effective_state_for_cell | read | SELECT cell state + SELECT proof states, pure computation |
| 3406 | task_state | read | `SELECT state FROM tasks` |
| 3422 | task_name_at | read | `SELECT name FROM tasks` |
| 3434 | task_project_at | read | `SELECT project FROM tasks` |
| 3450 | cell_sid_at | read | `SELECT sid FROM cells` |
| 3471 | cell_proof_running_from | read | `SELECT COUNT(*) FROM proofs` (running check) |
| 3490 | task_proof_running_from | read | `SELECT COUNT(*) FROM proofs` (running check) |
| 3505 | get_squad | read | SELECT-only via `build_squad_view` and its sub-helpers |
| 3537 | list_squads | read | SELECT-only via `build_squad_view` |
| 3573 | global_graph | read | calls `list_squads` + `squad_depends_on`, both read |
| 3928 | cell_prompts_for_review_branch | read | SELECT only |
| 3998 | proofs_for | read | SELECT only |
| 4041 | register_project | write | delegates to `register_project_with_clone_url_ex` (INSERT) |
| 4056 | register_project_ex | write | delegates to same INSERT-performing helper |
| 4077 | register_project_with_clone_url_ex | write | INSERT into `projects` |
| 4239 | clear_project_clone_url | write | `UPDATE projects SET clone_url=NULL` + `cartographer_log` |
| 4271 | project_skip_base_updates_stamp | read | delegates to `project_bool_stamp` (SELECT only) |
| 4279 | project_match_pr_branch_name_stamp | read | delegates to `project_bool_stamp` (SELECT only) |
| 4287 | project_auto_submit_pr_stack_stamp | read | delegates to `project_bool_stamp` (SELECT only) |
| 4295 | project_separate_pr_branch_stamp | read | delegates to `project_bool_stamp` (SELECT only) |
| 4306 | project_name_for_path | read | SELECT + in-memory prefix matching on the result, no field access |
| 4371 | load_all_project_stamps | read | SELECT only |
| 4400 | match_project_stamps | read | pure function, no `self`, no SQL/memory access |
| 4433 | get_project | read | `SELECT ... FROM projects WHERE name=?` |
| 4454 | list_projects | read | `SELECT ... FROM projects ORDER BY ...` |
| 4479 | resolve_project | read | calls `get_project`/`list_projects`, both read |
| 4521 | set_cell_cwd | write | `UPDATE cells SET cwd=...` |
| 4530 | guardian_worktree_records | read | SELECT only |
| 4556 | worktree_claims | read | SELECT only |
| 4589 | clear_guardian_worktree_path | write | two `UPDATE` statements |
| 4608 | record_guardian_worktree_retirement | write | `INSERT ... ON CONFLICT DO UPDATE` |
| 4642 | guardian_worktree_retirements | read | SELECT only |
| 4666 | guardian_display_name | read | SELECT only |
| 4676 | set_cell_effective_system_prompt | write | `UPDATE cells SET effective_system_prompt=...` |
| 4693 | get_cell_materialized_env_overrides | read | SELECT only |
| 4712 | set_cell_materialized_env_overrides | write | `UPDATE cells SET materialized_env_overrides=...` |
| 4728 | set_proof_effective_system_prompt | write | `UPDATE proofs SET effective_system_prompt=...` |
| 4746 | get_proof_materialized_env_overrides | read | SELECT only |
| 4768 | set_proof_materialized_env_overrides | write | `UPDATE proofs SET materialized_env_overrides=...` |
| 5242 | cells_of | read | SELECT only |
| 5283 | tasks_of | read | SELECT only |
| 5303 | task_commit_guard_info | read | SELECT only |
| 5324 | squad_depends_on | read | SELECT only |
| 5344 | add_squad_dependency | write | reads then `UPDATE squads SET depends_on=...` + `log_event` |
| 5401 | task_indices | read | SELECT only |
| 5414 | all_tasks_done | read | SELECT only |
| 5430 | proof_specs | read | SELECT only |
| 5461 | proof_state | read | SELECT only |
| 5480 | set_proof_state | write | `UPDATE proofs SET state=...` + `cartographer_log` |
| 5543 | set_proof_result | write | reads old state then `UPDATE proofs ...` + `log_event` |
| 5612 | edit_squad_label | write | `UPDATE squads` label, then notify_watchers |
| 5634 | edit_task_fields | write | `UPDATE tasks name/project/model` |
| 5679 | edit_proof_fields | write | `UPDATE proofs model/max_tool_output_tokens` |
| 5736 | edit_cell_fields | write | SELECT cell row, then `UPDATE cells` fields |
| 5836 | get_squad_env_overrides | read | SELECT `env_overrides` from squads |
| 5855 | set_squad_env_overrides | write | reads current map then `UPDATE squads env_overrides` |
| 5894 | get_task_env_overrides | read | SELECT `env_overrides` from tasks |
| 5916 | set_task_env_overrides | write | `UPDATE tasks` + `UPDATE cells` (out-of-date flag) |
| 5950 | get_task_proof_env_overrides | read | SELECT `proof_env_overrides` from tasks |
| 5973 | set_task_proof_env_overrides | write | `UPDATE tasks` + `UPDATE proofs` (out-of-date flag) |
| 6008 | get_cell_env_overrides | read | SELECT `env_overrides` from cells |
| 6031 | set_cell_env_overrides | write | `UPDATE cells` + `UPDATE proofs` (out-of-date flag) |
| 6066 | get_cell_proof_env_overrides | read | SELECT `proof_env_overrides` from cells |
| 6090 | set_cell_proof_env_overrides | write | `UPDATE cells` + `UPDATE proofs` (out-of-date flag) |
| 6126 | resolve_cell_env_overrides | read | composes 3 read getters |
| 6148 | resolve_cell_env_overrides_batch | read | batched SELECTs on squads/tasks/cells only |
| 6233 | resolve_task_proof_env_overrides | read | composes 3 read getters |
| 6246 | resolve_cell_proof_env_overrides | read | calls `resolve_cell_env_overrides` + one read getter |
| 6262 | get_proof_step_env_overrides | read | SELECT `env_overrides` from proofs |
| 6293 | set_proof_step_env_overrides | write | `UPDATE proofs` env_overrides + out-of-date flag |
| 6336 | resolve_task_proof_step_env_overrides | read | composes two read methods |
| 6349 | resolve_cell_proof_step_env_overrides | read | composes two read methods |
| 6373 | reset_squad_to_pending | write | `UPDATE squads/tasks/cells/proofs` to pending |
| 6401 | recover_orphaned_squads | write | SELECT running squads, then UPDATE all + `cartographer_log` |
| 6452 | done_cells_with_failed_proof | read | SELECT-only correlated subqueries |
| 6484 | done_cells | read | SELECT only |
| 6514 | failed_cells | read | SELECT only |
| 6529 | ignored_cells | read | SELECT only |
| 6556 | cancelled_cells | read | SELECT only |
| 6575 | cancelled_tasks | read | SELECT only |
| 6592 | squad_terminal_state | read | associated fn (no `&self`), pure bool -> enum logic; trivially safe on any connection or none |
| 6622 | reconcile_squad_cancellation | mixed | reads squad/task states, then conditionally calls `set_squad_state` (write) |
| 6657 | compute_squad_restart_impact | read | delegates only to read helpers (`cells_of`/`tasks_of`/`compute_dirty_dependents`) |
| 6700 | cancel_squad | mixed | read-only when `dry_run=true`; otherwise conditionally writes state/cancel/log |
| 6729 | restart_squad | write | unconditionally calls `reset_squad_to_pending` + `log_event` + `apply_dirty_dependents` |
| 6743 | compute_cell_restart_impact | read | delegates only to read helpers, same pattern as `compute_squad_restart_impact` |
| 6823 | restart_cell | write | direct `UPDATE cells/proofs/tasks/squads` |
| 6868 | compute_task_restart_impact | read | delegates only to read helpers |
| 6950 | restart_task | write | direct `UPDATE tasks/proofs/squads` |
| 7039 | apply_restart_user_note | write | computes targets (read) then unconditionally writes a ghost note per target |
| 7061 | compute_dirty_dependents | read | SELECT-only BFS over `squads.depends_on` |
| 7102 | apply_dirty_dependents | write | loops calling `reset_squad_to_pending` + `log_event`, both write, unconditionally |
| 7120 | dirty_dependents | mixed | reads dirty dependents then unconditionally applies them via `apply_dirty_dependents` (write) |
| 7129 | delete_squad | write | transactional DELETE across events/proofs/cells/tasks/etc. |
| 7171 | clear_all | write | transactional DELETE (all-or-filtered) across squad-family tables |
| 7249 | set_cell_review_branch | write | `UPDATE cells.review_branch` |
| 7271 | set_cell_review_guardian | write | `UPDATE cells.review_guardian_id` |
| 7314 | record_cell_result | write | `UPDATE cells` outcome/usage/finished_at_ms |
| 7359 | get_cell_id | read | `SELECT sid FROM cells` |
| 7374 | get_cell_agent | read | `SELECT agent FROM cells` |
| 7392 | get_proof_agent | read | `SELECT agent FROM proofs` |
| 7417 | get_task_cell_ids | read | `SELECT idx, sid FROM cells` |
| 7435 | get_task_name | read | `SELECT name FROM tasks` |
| 7458 | get_cell_agent_resume | read | `SELECT cwd, agent, agent_session_id` |
| 7485 | mark_cell_detached | write | `UPDATE cells.detached_at_ms` |
| 7498 | clear_cell_detached | write | `UPDATE cells.detached_at_ms=NULL` |
| 7517 | resume_detached_cell | write | existence check, then `UPDATE cells` + `log_event` |
| 7552 | set_force_resume_own_session | write | `UPDATE cells.force_resume_own_session=1` |
| 7572 | take_force_resume_own_session | write | reads flag then conditionally clears it via `UPDATE` |
| 7607 | get_cell_input_gate | read | SELECT machine/command/state, no writes |
| 7640 | cell_entity_uri | read | SELECT via join, no writes |
| 7671 | task_entity_uri | read | `SELECT idx FROM tasks` |
| 7703 | set_cell_agent_session_id_live | write | `UPDATE cells.agent_session_id` |
| 7732 | set_cell_live_usage | write | `UPDATE cells` token/cost fields |
| 7778 | note_live_activity | memory (write) | inserts into `live_activity`, no SQL |
| 7786 | live_activity_ms | memory (read) | reads `live_activity`, no SQL |
| 7795 | clear_live_activity | memory (write) | removes from `live_activity`, no SQL |
| 7807 | is_stall_escalated | memory (read) | reads `stall_escalated`, no SQL |
| 7815 | note_stall_escalated | memory (write) | inserts into `stall_escalated`, no SQL |
| 7823 | clear_stall_escalated | memory (write) | removes from `stall_escalated`, no SQL |
| 7842 | request_final_summary | memory (write) | mutates `guardian_summary_debounce` entry, no SQL |
| 7860 | take_due_final_summary_requests | memory (write) | drains due entries from `guardian_summary_debounce`, no SQL |
| 7883 | request_auto_submit_branch | write | `INSERT ... ON CONFLICT UPDATE guardian_auto_submit_requests` |
| 7895 | take_due_auto_submits | write | transactional SELECT + DELETE `guardian_auto_submit_requests` |
| 7918 | mark_final_summary_generated | memory (write) | sets `generated_signature` in the debounce map, no SQL |
| 7937 | claim_final_summary_repair | memory (write) | sets `repair_attempted` flag in the debounce map, no SQL |
| 7955 | get_task_first_cell_cwd | read | `SELECT cwd FROM cells ORDER BY idx LIMIT 1` |
| 7976 | get_proof_agent_session_id | read | `SELECT agent, agent_session_id FROM proofs` |
| 8057 | restart_cell_proof | write | `UPDATE proofs/tasks/squads` + `revive_failed_downstream_cells` (write) + `log_event` |
| 8109 | restart_task_proof | write | `UPDATE proofs/tasks/squads` + `revive_failed_downstream_cells` (write) + `log_event` |
| 8164 | cells_needing_proof_only | read | `SELECT DISTINCT` with `EXISTS` subquery |
| 8190 | queue | read | composite of SELECT-only helpers (`cells_of`/`tasks_of`/`queue_items_for_squad`), no writes |
| 8469 | set_queue_rank | write | `UPDATE cells/proofs .queue_rank` by path kind |
| 8493 | reorder_queue | mixed | reads `queue()` then writes `queue_rank` per item + `cartographer_log` |
| 8528 | set_queue_position | mixed | reads `queue()` then delegates to `reorder_queue` (write) + `cartographer_log` |

## Summary

| category | count |
|---|---|
| read | 89 |
| write | 54 |
| memory (read) | 3 |
| memory (write) | 13 |
| mixed | 9 |
| **total** | **168** |

(The ticket's "~161" was an estimate; the true count once
`SquadState`/`NodeState` enum methods and free functions are excluded is
168.)

### Pooling priority among `read`-classified methods

**Already pooled** (RAL-393 Stage 3, shipped with this change):
`running_count` / `running_count_conn`, exercised by `GET /api/daemon`
(`Daemon::read_running_count`) -- the exact endpoint the ticket measured at
0.38s-4.2s p95 under writer contention, now proven under 100ms by
`server::tests::api_daemon_health_reads_stay_fast_and_busy_free_under_writer_contention`.

**Best next candidates** -- pure `self.conn`-only reads, reachable directly
from a `GET` handler, no delegation through multi-layer view-builders:
`list_projects`, `get_project`, `running_cell_count`, `merging_guardians`,
`squad_state`, `cell_state`, `task_state`, `events_for_squad`,
`events_for_guardian`, `get_squad_env_overrides` and its sibling
`get_*_env_overrides` getters. Each needs only the same `_conn`-split
refactor `running_count`/`running_count_conn` already demonstrates, plus a
`Daemon`-level "try the pool, fall back to `.lock()`" wrapper like
`read_running_count`.

**Lower priority / defer** -- `read`-classified methods that are either
internal-only (never reached directly from an HTTP handler; called only by
the scheduler, guardian-merge workers, or other `Store` methods while
already holding other work) or multi-layer composites (`get_squad`,
`list_squads`, `queue`, `resolve_cell_env_overrides_batch`) whose SQL is
spread across several private helper functions each needing their own
`_conn` split before the top-level method could move to the pool. Moving
these is valuable follow-up work but not required to fix the starvation
this ticket measured, which was specific to `GET /api/daemon`.

### `mixed` and `memory`-mutation methods

None of the 9 `mixed` or 13 `memory (write)` methods can move to
`ReadConnPool` regardless of priority: `mixed` methods have a live write
branch, and `memory` mutations touch fields the pool has no access to at
all (the pool only opens independent `rusqlite::Connection`s -- it does not
share `Store`'s Rust-level state). Both categories are correctly staying on
`StoreMutex`.
