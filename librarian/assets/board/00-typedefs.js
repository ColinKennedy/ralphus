      // ---- Shared type definitions (JSDoc-only; see docs/daemon-api.md for the
      // authoritative wire shapes). Checked via `npm run typecheck` (tsc --checkJs
      // over the extracted <script> body) — see tsconfig.board.json. ----
      /**
       * @typedef {object} ProofView
       * @property {string|null} id
       * @property {string} kind - "command" | "prompt" | "brain" | "approval"
       * @property {string} state - "pending" | "running" | "done" | "failed" | "cancelled"
       * @property {number|null} [delayed_until_ms] - Epoch-ms when an agent-backed proof step will automatically retry after a provider rate limit; absent otherwise.
       * @property {string|null} [delayed_reason] - Safe provider-delay summary shown while `delayed_until_ms` is set.
       * @property {string|null} output
       * @property {string} spec
       * @property {string|null} [system_prompt]
       * @property {string|null} model
       * @property {string} [agent]
       * @property {string} [agent_session_id]
       * @property {number} [tokens_in]
       * @property {number} [tokens_out]
       * @property {number} [cost_usd]
       * @property {number} [cache_creation_tokens] - RAL-326: prompt-cache write tokens (input billed at the cache-creation rate). Kept out of `tokens_in`, which still means uncached input only. 0 for a backend whose harness reports no cache breakdown.
       * @property {number} [cache_read_tokens] - RAL-326: prompt-cache read tokens (input served from an existing cache entry). See `cache_creation_tokens`.
       * @property {number} [compaction_input_tokens] - RAL-373: input tokens spent on Claude Code's own auto-compaction summarization requests, billed at the uncached input rate. Already included in `cost_usd` -- a derived slice of it, never additional spend. 0 for a backend that reports no compaction data (`pi`, `codex`) -- see `compaction_count` before reading that as "never compacted".
       * @property {number} [compaction_count] - RAL-373: count of compactions observed, independent of whether each one's input size was reported. Nonzero here with `compaction_input_tokens` still 0 means "compactions happened, sizes unreported by this backend/version", not "no compaction happened".
       * @property {number} [turns] - RAL-352: completed user/assistant message exchanges (each response event counts as both sides of one exchange). Absent (not 0) for a command-mode step -- there is no conversational count at all -- and for any pre-RAL-352 row that hasn't re-run yet.
       * @property {boolean} [cost_is_estimated] - RAL-326: true when the tokens/cost above are the last live mid-run snapshot rather than the backend's own final accounting, because the process was lost/cancelled/timed out before a terminal usage event.
       * @property {{[key: string]: string}} [env_overrides] - RAL-191: environment-variable overrides set on this individual proof step — the narrowest layer, merged on top of the owning scope's `proof_env_overrides` and its ancestors. Absent for the vast majority of steps.
       * @property {boolean} [env_out_of_date] - RAL-271: cosmetic "out of date" badge — true once this step's own `env_overrides` (or its owning scope's `proof_env_overrides`) has been edited since the step last ran/retried or had its status explicitly set. No behavioral effect.
       * @property {number} [maximum_tool_output_tokens] - RAL-333: resolved tool-output token cap, or absent for no cap. Resolved against this step's real parent (owning cell for a cell-scope step, owning task for a task-scope step) -- see `agent_supports_maximum_tool_output_tokens`.
       */
      /**
       * @typedef {object} CellView
       * @property {string} id
       * @property {string} [name]
       * @property {string} cwd
       * @property {string} agent
       * @property {string|null} model
       * @property {string} state
       * @property {number} [tokens_in]
       * @property {number} [tokens_out]
       * @property {number} [cost_usd]
       * @property {number} [cache_creation_tokens] - RAL-326: prompt-cache write tokens (input billed at the cache-creation rate). Kept out of `tokens_in`, which still means uncached input only. 0 for a backend whose harness reports no cache breakdown.
       * @property {number} [cache_read_tokens] - RAL-326: prompt-cache read tokens (input served from an existing cache entry). See `cache_creation_tokens`.
       * @property {number} [compaction_input_tokens] - RAL-373: input tokens spent on Claude Code's own auto-compaction summarization requests, billed at the uncached input rate. Already included in `cost_usd` -- a derived slice of it, never additional spend. 0 for a backend that reports no compaction data (`pi`, `codex`) -- see `compaction_count` before reading that as "never compacted".
       * @property {number} [compaction_count] - RAL-373: count of compactions observed, independent of whether each one's input size was reported. Nonzero here with `compaction_input_tokens` still 0 means "compactions happened, sizes unreported by this backend/version", not "no compaction happened".
       * @property {number} [turns] - RAL-352: completed user/assistant message exchanges (each response event counts as both sides of one exchange). Absent (not 0) for a command cell -- there is no conversational count at all -- and for any pre-RAL-352 cell that hasn't re-run yet.
       * @property {boolean} [cost_is_estimated] - RAL-326: true when the tokens/cost above are the last live mid-run snapshot rather than the backend's own final accounting, because the process was lost/cancelled/timed out before a terminal usage event.
       * @property {number} [maximum_budget_usd]
       * @property {number} [maximum_context] - RAL-304: resolved context-window token limit, or absent for no cap. Only ever set for a backend with a real delivery mechanism (codex, pi) -- see `agent_supports_maximum_context`.
       * @property {number} [auto_compact_threshold] - RAL-304: resolved auto-compact trigger threshold in tokens, or absent for no explicit threshold. Accepted by a wider set of backends than `maximum_context` (codex, pi, and claude-code) -- see `agent_supports_auto_compact_threshold`.
       * @property {number} [maximum_tool_output_tokens] - RAL-333: resolved tool-output token cap (cell overrides task), or absent for no cap -- see `agent_supports_maximum_tool_output_tokens`.
       * @property {string|null} [error]
       * @property {ProofView[]} [proof]
       * @property {string[]} [depends_on]
       * @property {string} [command]
       * @property {string} [prompt]
       * @property {string|null} [system_prompt]
       * @property {string} [agent_session_id]
       * @property {string} [machine] - RAL-185/RAL-288: where this cell is routed. Absent/undefined means the daemon's own host.
       * @property {{id: string, name: string, status: string, branch?: string, origin: string}[]} [reviews]
       * @property {string[]} [triage_types] - RAL-318: this cell's resolved Triage type(s) if it opted in via `triage = true` in its TOML -- absent/empty for a cell that never opted in. Populated at submit time (inline `triage_type` or the Arbiter's own classification) whether or not the cell has run yet, so non-empty here does not by itself mean the cell is done -- check `state`. A cell with types set here and no entry in `reviews` yet is pooled, waiting for its Triage pool to drain into an actual review.
       * @property {string[]} [subprojects] - RAL-346: this cell's resolved monorepo subproject(s), if any -- absent/empty when unresolved (a monorepo cell the Arbiter hasn't matched yet) or not applicable (a plain single-project repo). Used to key this cell's Triage pool by (project, subproject) rather than just project.
       * @property {boolean} [subprojects_inferred] - RAL-346: true when `subprojects` was written by the Arbiter's async description-matching inference step rather than seeded from the cell's own manually-declared `subprojects` TOML field. Meaningless when `subprojects` is empty. Drives the board's purple "Arbiter set this" badge (`subprojectBadge`).
       * @property {{[key: string]: string}} [env_overrides] - Persistent environment-variable overrides set directly on this cell (hierarchical env overrides, extending RAL-150); merged on top of the owning task's/squad's when the cell's own subprocess is spawned. Empty/absent for the vast majority of cells.
       * @property {{[key: string]: string}} [proof_env_overrides] - Persistent environment-variable overrides applied only to this cell's own proof steps, merged on top of `env_overrides` (and its ancestors).
       * @property {number|null} [started_at_ms]
       * @property {number|null} [finished_at_ms]
       * @property {boolean} [env_out_of_date] - RAL-271: cosmetic "out of date" badge — true once this cell's own `env_overrides` has been edited since the cell last ran/retried or had its status explicitly set. No behavioral effect.
       * @property {number|null} [detached_at_ms] - RAL-288: when this cell was cleanly stopped for a real interactive agent session to take over, or absent/null while it isn't detached. State still reads `running` while this is set — the cell is paused for a human, not stuck. Clears automatically on the cell's next dispatch (a restart, or resume-automation).
       * @property {number|null} [delayed_until_ms] - RAL-435: Unix epoch milliseconds this cell expects to automatically resume at, or absent/null while it isn't waiting out a rate limit. State still reads `running` while this is set — the daemon is waiting out a recognized, retryable provider rate limit's suggested delay before resuming the same agent session, not stuck and not waiting on a human (contrast `detached_at_ms`). Clears automatically once the delay elapses (a successful resume) or the retry thrashes and the cell fails.
       */
      /**
       * @typedef {object} TaskView
       * @property {string} name
       * @property {string} project - Registered project name, or a server-derived fallback (cwd basename, or "unassigned") when the task's TOML left `project` unset (RAL-141). Always present.
       * @property {string|null} agent - Raw task-level `agent` from the submitted TOML, or null when unset.
       * @property {string|null} model - Raw task-level `model` from the submitted TOML, or null when unset.
       * @property {string} state
       * @property {string|null} [error] - RAL-291: failure detail for a task-level failure with no underlying cell/proof error to point to (e.g. the RAL-156 no-commits-since-baseline guard). Absent/null when the task hasn't failed this way, including when a child cell/proof failure caused the task to fail instead.
       * @property {CellView[]} cells
       * @property {ProofView[]} [proof]
       * @property {string[]} [depends_on]
       * @property {{[key: string]: string}} [env_overrides] - Persistent environment-variable overrides set directly on this task (hierarchical env overrides, extending RAL-150); merged on top of the squad's, and merged onto every cell under this task. Empty/absent for the vast majority of tasks.
       * @property {{[key: string]: string}} [proof_env_overrides] - Persistent environment-variable overrides applied only to this task's own (task-scoped) proof steps, merged on top of `env_overrides` (and the squad's).
       * @property {boolean} soloed
       * @property {number|null} [started_at_ms]
       * @property {number|null} [finished_at_ms]
       * @property {boolean} [env_out_of_date] - RAL-271: cosmetic "out of date" badge — true once this task's own `env_overrides` has been edited since the task last ran/retried or had its status explicitly set. No behavioral effect.
       */
      /**
       * One cell's derived git info from `GET /api/squads/{id}/worktrees`
       * (CCTL-148; `upstream` added later) — the cell's own worktree
       * (`cwd`), its project, and its read-only display upstream.
       * @typedef {object} CellPathInfo
       * @property {number} task_idx
       * @property {number} cell_idx
       * @property {string|null} worktree
       * @property {string|null} project - RAL-396: the registered project's name when this cell's worktree maps to one, otherwise the derived (unregistered) repo path as a fallback. Resolved server-side — never a raw path to display when a project association exists, since a cell's on-disk path isn't stable for remote-machine cells (RAL-185).
       * @property {string|null} upstream
       */
      /**
       * One durably-persisted terminal-log attempt's metadata (RAL-154), from
       * `GET .../terminal-log-attempts` — `attempt` `0` is the initial run,
       * `1..` each subsequent tmux reattach.
       * @typedef {object} AttemptMeta
       * @property {number} attempt
       * @property {number} size_bytes
       * @property {number} modified_ms
       */
      /**
       * @typedef {object} RawTranscriptRange - one byte range of a `.raw`
       *   pipe-pane transcript returned by a `.../pane-transcript` endpoint
       *   (RAL-397 Phase 2F); see docs/daemon-api.md.
       * @property {string} content - The decoded byte range, raw (ANSI escapes included; re-encoded UTF-8 lossy at either edge).
       * @property {number} start - Byte offset `content` actually begins at (clamped to the file's current size).
       * @property {number} total - Transcript size in bytes at read time.
       */
      /**
       * One `GET .../system-prompt` reply (RAL-428) — the exact effective
       * system prompt an agent step received, served for the admin-only
       * System Prompt tab in the live terminal viewer. `system_prompt`
       * holds the full text when `available`; `reason` explains absence (a
       * command cell, or a prompt step never yet dispatched).
       * @typedef {object} SystemPromptReply
       * @property {boolean} available
       * @property {string} [system_prompt]
       * @property {string} [reason]
       */
      /**
       * @typedef {object} SquadView
       * @property {string} id
       * @property {string|null} label
       * @property {string} state
       * @property {number} created_at_ms
       * @property {number|null} [started_at_ms]
       * @property {number|null} [finished_at_ms]
       * @property {TaskView[]} tasks
       * @property {string[]} [projects] - Every distinct `TaskView.project` among this squad's tasks, precomputed server-side so the sidebar's project filter needs no per-task data.
       * @property {{id: string, name: string, status: string}[]} [reviews]
       * @property {{[key: string]: string}} [env_overrides] - Persistent environment-variable overrides (RAL-150); applied to every cell/proof subprocess this squad spawns from now on, until unset. Empty/absent for the vast majority of squads.
       * @property {GenerationUsage} [generation_cost] - RAL-420: this squad's own pre-work generation cost — the retained usage of the agent/model calls the Simple form made before submission (Generate proof steps / manual checks / auto-build steps, plus the suggest-name fallback), attributed to this squad at submit time and folded into its normal totals exactly once. Absent for the vast majority of squads (only Simple-tab submissions that used the Generate buttons or the suggest-name fallback have rows).
       */
      /**
       * RAL-420: a squad's aggregate pre-work generation cost — sums over its
       * attributed `squad_generation_costs` rows. Absent from the wire for a
       * squad with none.
       * @typedef {object} GenerationUsage
       * @property {number} count
       * @property {number} tokens_in
       * @property {number} tokens_out
       * @property {number} cache_creation_tokens - prompt-cache write tokens; kept out of `tokens_in` (which stays uncached-input-only, same as CellView).
       * @property {number} cache_read_tokens - prompt-cache read tokens; see `cache_creation_tokens`.
       * @property {number} cost_usd
       * @property {boolean} estimated - true once any contributing call's figures are a live mid-run snapshot rather than the backend's final accounting (the call was killed/cancelled/sat before a terminal usage event) — RAL-326's flag, aggregated.
       */
      /**
       * One retained pre-work generation call (RAL-420), as served by
       * `GET /api/squads/{id}/generation-costs` and `GET /api/generation-costs`.
       * Rows exist from the moment the call finishes (whether it succeeded,
       * failed, or was cancelled); `squad_id` fills in only once the call is
       * attributed to a submitted squad, and stays null for a call whose
       * squad was never submitted (visible in the cross-squad audit list only).
       * @typedef {object} GenerationCostView
       * @property {number} id
       * @property {string} job_id
       * @property {string|null} squad_id
       * @property {string} kind - "proof_steps" | "manual_checks" | "auto_build_steps" | "task_name"
       * @property {string} status - "done" | "error" | "cancelled"
       * @property {number} tokens_in
       * @property {number} tokens_out
       * @property {number} cache_creation_tokens
       * @property {number} cache_read_tokens
       * @property {number} cost_usd
       * @property {boolean} cost_is_estimated
       * @property {string|null} error
       * @property {string} agent
       * @property {string|null} model
       * @property {number} created_at_ms
       * @property {number} finished_at_ms
       * @property {number|null} attributed_at_ms
       */
      /**
       * @typedef {object} DaemonStatus
       * @property {number} running
       * @property {number} max_concurrent - The global concurrency cap; `0` means no limit.
       * @property {string[]} running_reviews
       */
      /**
       * @typedef {object} GuardianBranch
       * @property {string} id - Stable, globally-unique branch identity (RAL-122); use this to address a branch across requests, not `position`.
       * @property {string} branch
       * @property {number} position - Display/reorder order only; changes when the stack is reordered.
       * @property {boolean} [enabled]
       * @property {string} [name]
       * @property {string} [project]
       * @property {string} [merge_status] - "pending" | "in_progress" | "done" | "proof_pending" | "conflict_resolved" | "failed" | "merged" | "closed" | ...
       * @property {number|null} [delayed_until_ms] - Epoch-ms when this review worktree's agent call will retry after a provider rate limit.
       * @property {string|null} [delayed_reason] - Safe provider-delay summary shown while `delayed_until_ms` is set.
       * @property {string} [moved_from_guardian_id]
       * @property {string} [source_squad_id]
       * @property {number} [source_task_idx]
       * @property {number} [source_cell_idx]
       * @property {string} [source_cell_state]
       * @property {string} [worktree]
       * @property {string} [detail]
       * @property {number} [conflicts_found]
       * @property {number} [conflicts_fixed]
       * @property {number} [conflicts_committed]
       * @property {boolean} [is_empty] - Branch rebased cleanly but adds no diff over the branch beneath it, which fails the review (RAL-190).
       * @property {number|null} [rebase_commands_done] - Live rebase-todo position (RAL-145); null unless a rebase is actively paused/running in this branch's worktree.
       * @property {number|null} [rebase_commands_total] - Live rebase-todo total (RAL-145); null unless a rebase is actively paused/running in this branch's worktree.
       * @property {boolean} [can_reenable]
       * @property {string} [resolver_agent_session_id]
       * @property {{[key: string]: (string|null)}} [env_overrides] - RAL-191: this branch's own env-override layer. A string value overrides the inherited one; `null` is a tombstone removing the inherited variable entirely; an absent key is simply inherited.
       * @property {{[key: string]: string}} [inherited_env] - RAL-191: the environment inherited from this branch's source cell (`squad < task < cell`), before `env_overrides` is applied.
       * @property {{[key: string]: string}} [resolved_env] - RAL-191: the effective environment this branch's review worktree actually runs under — `inherited_env` with `env_overrides` applied.
       * @property {number|null} [started_at_ms] - RAL-259: epoch-ms when this branch's conflict-resolver agent (fix pass or final-proof call) most recently began running, or null if none has started. Persists after the resolver finishes.
       * @property {number|null} [finished_at_ms] - epoch-ms when this branch's conflict-resolver agent (fix pass or final-proof call) most recently finished running, or null if none has completed. Shown alongside `started_at_ms` once the Live View shows a historical record.
       * @property {string|null} [auto_submit_error] - RAL-317: error from this branch's most recent auto-submit-PR-stack attempt, or null/absent if none failed (or none has run). Cleared server-side once the branch's state is covered by an open PR again.
       * @property {boolean} [pr_submission_pending] - RAL-389: true while an auto-submit-PR-stack request for this branch is durably queued or actively running on its own async worker thread, decoupled from the merge worker. Survives a daemon restart; cleared once that attempt completes (success or failure).
       */
      /**
       * A named, defaulted value referenced by a GuardianCheck's
       * command/cleanup_command as a `{name}` placeholder (RAL-164).
       * @typedef {object} CheckInput
       * @property {string} name
       * @property {string} message
       * @property {string} default
       */
      /**
       * One runnable review check (RAL-164): either a user-declared
       * `[[review.action]]` hint (has a `label`) or an AI-synthesized manual
       * check (no `label`). Exactly one of `command`/`prompt` is set.
       * @typedef {object} GuardianCheck
       * @property {string} [label]
       * @property {string} [command]
       * @property {string} [prompt]
       * @property {string} [cleanup_command]
       * @property {CheckInput[]} [inputs]
       */
      /**
       * @typedef {object} BranchConflicts
       * @property {string[]} files - Paths (relative to the branch's worktree root) still carrying unresolved conflict markers. Empty once resolved, or if the worktree doesn't exist (yet, or anymore).
       * @property {boolean} rebase_in_progress - Whether the worktree is currently mid-`git rebase`. Can be false while `files` is still non-empty.
       */
      /**
       * Status of a "set it for me" AI resolution for one named CheckInput
       * (RAL-164). `value` is only present once `status` is "ready".
       * @typedef {object} InputResolution
       * @property {string} status - "resolving" | "ready" | "failed"
       * @property {string} [value]
       */
      /**
       * One squad, review, or (RAL-365) individual task a user has chosen
       * to hide from their own view (RAL-328/RAL-331/RAL-365). `squad_id`
       * is set for both "squad" and "task" kinds (a task's owning squad);
       * `task_idx` is set only for "task". A hidden task and its owning
       * hidden squad are independent rows -- see the daemon's
       * `hidden::hide_task` doc comment for the union rule.
       * @typedef {object} HiddenItem
       * @property {string} kind - "squad" | "review" | "task"
       * @property {string|null} squad_id
       * @property {string|null} guardian_id
       * @property {number|null} task_idx
       * @property {number} hidden_at_ms
       */
      /**
       * Response from `POST /api/hidden/squads/batch` (RAL-331) -- every
       * requested id that failed (e.g. already deleted), reported instead of
       * aborting the whole batch.
       * @typedef {object} HiddenBatchResult
       * @property {boolean} hidden
       * @property {{id: string, error: string}[]} failed
       */
      /**
       * Response from `POST /api/hidden/tasks/batch` (RAL-365) -- same idea
       * as `HiddenBatchResult`, but keyed by `(squad_id, task_idx)` pairs.
       * @typedef {object} HiddenTasksBatchResult
       * @property {boolean} hidden
       * @property {{squad_id: string, task_idx: number, error: string}[]} failed
       */
      /**
       * @typedef {object} GuardianView
       * @property {string} id
       * @property {string} name
       * @property {string} status
       * @property {string} [base_branch]
       * @property {string[]} [projects]
       * @property {string[]} [squash_projects]
       * @property {GuardianBranch[]} branches
       * @property {number} [branch_count] - not a real `GuardianView` field -- present only on a `guardians[]` entry that's still lean (came from `/api/guardian-index` and hasn't had this review's full detail merged in yet, see the `guardians` declaration in `05-engines.js`). `branches` is the authoritative count once available.
       * @property {string} [squad_id]
       * @property {string} [git_root]
       * @property {string|null} [project] - registered-project creation identity, or null for a raw-directory review
       * @property {string} [review_type]
       * @property {string} [review_branch]
       * @property {string} [combined_worktree]
       * @property {string[]} [checks]
       * @property {string} [checks_state] - "ready" | "generating" | "waiting"
       * @property {GuardianCheck[]} [manual_commands]
       * @property {string} [manual_commands_agent]
       * @property {string} [manual_commands_model]
       * @property {string} [manual_commands_agent_session_id]
       * @property {GuardianCheck[]} [action_hints]
       * @property {Record<string,string>} [input_values]
       * @property {Record<string,InputResolution>} [input_resolutions]
       * @property {string} [resolver_agent]
       * @property {string} [resolver_model]
       * @property {string} [summary_agent]
       * @property {string} [summary_model]
       * @property {string} [summary_state]
       * @property {string} [change_summary]
       * @property {string} [detail]
       * @property {boolean} [skip_auto_build]
       * @property {boolean} [skip_worktrees]
       * @property {number} [conflicts_found]
       * @property {number} [conflicts_fixed]
       * @property {number} [conflicts_committed]
       * @property {string|null} [proof_scope] - RAL-168: this review's own Proof-scope override ("each_branch"|"final_branch"|"nothing"), or null to inherit the project default.
       * @property {boolean|null} [proof_skip_auto_clean] - RAL-168: this review's own override for "each_branch"'s auto-clean-skip sub-option, or null to inherit the project default.
       * @property {string} [effective_proof_scope] - RAL-168: proof_scope resolved against the project default -- always "each_branch"|"final_branch"|"nothing".
       * @property {boolean} [effective_proof_skip_auto_clean] - RAL-168: proof_skip_auto_clean resolved against the project default.
       * @property {{[key: string]: string}} [combined_env] - RAL-203: the environment the combined worktree inherits by default -- the last enabled branch's own `resolved_env`. Shared baseline `build_env`/`manual_checks_env` each layer their own overrides on top of.
       * @property {{[key: string]: (string|null)}} [build_env_overrides] - RAL-203: this review's own env-override layer for the finalize-time build/check-gate step. A string value overrides the inherited one; `null` is a tombstone; an absent key is simply inherited.
       * @property {{[key: string]: string}} [build_env] - RAL-203: the effective environment the build/check-gate step runs under -- `combined_env` with `build_env_overrides` applied.
       * @property {{[key: string]: (string|null)}} [manual_checks_env_overrides] - RAL-203: this review's own env-override layer for the manual-checks step (the LLM-suggested commands run via `ralphus review checks run` / "Run all"). Independent of `build_env_overrides`.
       * @property {{[key: string]: string}} [manual_checks_env] - RAL-203: the effective environment the manual-checks step runs under -- `combined_env` with `manual_checks_env_overrides` applied.
       * @property {number|null} [maximum_budget_usd] - RAL-193: this review's own USD spend cap, or null for no cap.
       * @property {number} [merge_attempt] - RAL-193: current merge-attempt counter, bumped once per rebase/re-merge.
       * @property {number|null} [manual_checks_started_at_ms] - RAL-259: epoch-ms when this review's manual-checks generation agent most recently began work, or null if generation hasn't started yet. Persists after generation finishes.
       * @property {number|null} [manual_checks_finished_at_ms] - epoch-ms when this review's manual-checks generation agent most recently finished work, or null if generation hasn't completed yet. Shown alongside `manual_checks_started_at_ms` once the Live View shows a historical record.
       * @property {string|null} [post_merge_status] - the post-merge phase's rolled-up state: "running" while the check gates and/or manual-checks generation are still working against an already-finished stack, then "ok" or "failed"; null for a review that has never completed a merge. The review's own status is `in_review` throughout — a merge is complete once its branches are rebased, and these jobs run after it. "failed" is advisory and never blocks approval or PR submission.
       * @property {string|null} [post_merge_detail] - the post-merge phase's note: what failed when `post_merge_status` is "failed", otherwise the gate's own summary (e.g. which build command ran). Null when there is nothing to report.
       * @property {number|null} [post_merge_started_at_ms] - epoch-ms when the post-merge phase most recently started, or null if it has never run.
       * @property {number|null} [post_merge_finished_at_ms] - epoch-ms when the post-merge phase most recently finished, or null while it is still running.
       * @property {number} [attempt_tokens_in] - RAL-193: input tokens spent on this review's own resolver/proof calls during the current merge attempt only.
       * @property {number} [attempt_tokens_out] - RAL-193: output tokens, current merge attempt only.
       * @property {number} [attempt_cost_usd] - RAL-193: USD cost, current merge attempt only.
       * @property {number} [cumulative_tokens_in] - RAL-193: input tokens spent on this review's own resolver/proof calls, cumulative across every rebase/re-merge attempt.
       * @property {number} [cumulative_tokens_out] - RAL-193: output tokens, cumulative across every attempt.
       * @property {number} [cumulative_cost_usd] - RAL-193: USD cost, cumulative across every attempt -- what maximum_budget_usd is enforced against.
       * @property {string|null} [notice_kind] - RAL-273: a one-shot, GUI-facing notice, e.g. "forge_reorder_interrupted_local". Null when there is nothing to show.
       * @property {string|null} [notice_message] - RAL-273: human-readable text for `notice_kind`.
       * @property {number|null} [notice_at_ms] - RAL-273: when `notice_kind` was recorded (epoch ms); the board toasts once per new value it observes.
       * @property {boolean|null} [match_pr_branch_name] - RAL-307: this review's own override for whether a newly submitted PR's branch defaults to the worktree/feature branch name, or null to inherit the project/global default. Stamped from the project's effective value at review creation.
       * @property {boolean} [effective_match_pr_branch_name] - RAL-307: match_pr_branch_name resolved against the project/global default -- what PR submission actually gates on unless a per-submission override is passed.
       * @property {boolean|null} [separate_pr_branch] - RAL-378: this review's own override for whether its pull request is pushed to a branch separate from its review branch, or null to inherit the project/global default. Stamped from the project's effective value at review creation.
       * @property {boolean} [effective_separate_pr_branch] - RAL-378: separate_pr_branch resolved against the project/global default. False (the default) means the PR is opened from the review branch itself, and both match_pr_branch_name and the branch convention are ignored.
       * @property {boolean|null} [dual_root_pr] - RAL-<new>: fork-routed only. This review's own override for whether its stack root branch (and whichever branch later gets promoted to root) gets a second, same-repo "stack" PR into a mirror of the parent's base branch, or null to inherit the project/global default. Stamped from the project's effective value at review creation.
       * @property {boolean} [effective_dual_root_pr] - RAL-<new>: dual_root_pr resolved against the project/global default. False (the default) means today's single-PR-per-root behavior, unchanged.
       * @property {boolean} [readable_review_branch] - RAL-378: whether this review's combined worktree branch is named readably rather than as the internal guardian/<id>/review ref. False for reviews created before readable naming landed.
       * @property {string|null} [review_branch_name] - RAL-378: the sticky readable name claimed for this review's combined worktree branch, derived from its name at the first combined build.
       * @property {boolean|null} [auto_submit_pr_stack] - RAL-317: this review's own override for whether the PR stack is auto-submitted/grown as each branch reaches a terminal merge state, or null to inherit the project/global default. Stamped from the project's effective value at review creation.
       * @property {boolean} [effective_auto_submit_pr_stack] - RAL-317: auto_submit_pr_stack resolved against the project/global default -- what the per-branch auto-submit trigger actually gates on.
       * @property {boolean|null} [auto_fix_pr_errors] - RAL-395: this review's own override for whether the resolver agent is auto-dispatched to fix this review's PR when its CI checks go red, or null to inherit the project/global default. No `effective_` counterpart is exposed yet -- callers read this raw value.
       * @property {string|null} [auto_fix_prompt_template] - RAL-395: this review's own prompt template for that auto-fix dispatch, with `<<prompt>>` replaced by the failing branch's own Cell prompts, or null to inherit the project default. Non-empty values must contain the literal `<<prompt>>` placeholder -- enforced server-side.
       * @property {boolean|null} [discourage_tests_during_auto_pull_request_fixes] - RAL-505: this review's own override for whether the resolver agent dispatched to fix this review's PR (auto-fix or a manual PR-fix request) is told to prefer automatic formatters/linters/static analysis and avoid a broad or expensive test suite, or null to inherit the project/global default. No `effective_` counterpart is exposed yet -- callers read this raw value.
       * @property {string} [origin] - RAL-318: provenance of this review -- "explicit" (an authored [[review]] block, or any other pre-existing creation path -- the default/normal case) or "arbiter" (created automatically by the Arbiter/Triage subsystem when a pooled cell count threshold or cron schedule fired).
       */
      /**
       * `GET /api/guardian-index` response shape -- the lean per-review
       * summary the Reviews tab's sidebar list, its filters, and
       * `checkGuardianNotices` read. Everything a `GuardianView` carries
       * beyond these fields (env overrides, resolver session ids, token/cost
       * accounting, per-branch detail, ...) is fetched separately, per
       * review, only once that review is actually opened -- see
       * `guardianDetail`.
       * @typedef {object} GuardianIndexEntry
       * @property {string} id
       * @property {string} name
       * @property {string} status
       * @property {string} origin
       * @property {number} branch_count
       * @property {string|null} resolver_agent
       * @property {string} git_root
       * @property {string[]} projects
       * @property {string|null} notice_kind
       * @property {string|null} notice_message
       * @property {number|null} notice_at_ms
       */
      /**
       * A pull/merge request submitted for one of a review's branches, or for
       * its combined (all-branches) worktree when `branch_id` is null (RAL-117/RAL-190).
       * @typedef {object} PullRequestView
       * @property {string} id
       * @property {string} guardian_id
       * @property {string|null} [branch_id]
       * @property {string} forge - "github" | "gitlab"
       * @property {string} repo
       * @property {string} branch_alias - The branch name actually pushed to the remote.
       * @property {string} base_ref
       * @property {string} title
       * @property {string} description
       * @property {number|null} [pr_number]
       * @property {string|null} [pr_url]
       * @property {string} state - "open" | "merged" | "closed" | "dropped"
       * @property {number} created_at_ms
       * @property {number} updated_at_ms
       * @property {string|null} [last_pushed_sha]
       * @property {string|null} [stack_id]
       * @property {string|null} [dropped_reason]
       * @property {string|null} [superseded_by] - RAL-338: the id of the PR row that replaced this one (fork-promotion reconcile-first). Non-null means a fresher row is the current one to show, not this one.
       * @property {string|null} [ci_status] - RAL-395: "pending" | "passing" | "failing", from the last standing CI/CD poll. null if never polled.
       * @property {string|null} [ci_failure_job_url] - RAL-395: the failing job's forge URL, when `ci_status === "failing"` and the forge gave one.
       * @property {number} [auto_fix_attempt_count] - Number of unattended CI-fix attempts in the current failing campaign.
       * @property {number|null} [auto_fix_next_attempt_at_ms] - UTC epoch-ms when the next backoff-limited unattended attempt may run.
       * @property {string|null} [auto_fix_error] - Human-readable reason unattended CI fixing has stopped; null while it remains eligible.
       * @property {boolean|null} [draft] - RAL-353: whether the forge reports this PR/MR as a draft (WIP). null only for rows recorded before the column existed and never polled since; the board treats null as not-draft.
       * @property {string} [pr_kind] - RAL-<new>: "parent" (the default, and every pre-dual_root_pr row) | "stack". A "stack" row is a fork-routed root branch's second, same-repo PR into a mirror of the parent's base branch (dual_root_pr mode) -- it visually chains the branch into the rest of the stack, is never expected to merge, and closes once the branch's "parent" PR does.
       * @property {string|null} [auto_fix_last_outcome] - RAL-509: the latest reason unattended auto-fix did or did not run for this PR (e.g. "deferred_no_worktree", "deferred_backoff", "exhausted", "auto_fix_passed"); null if auto-fix has never evaluated this PR.
       */
      /**
       * One past "submit a stack" call for a review (RAL-302): every PR row
       * it created, in any state -- see
       * `GET /api/guardians/{id}/pull-request-stacks`.
       * @typedef {object} PrStackView
       * @property {string} stack_id
       * @property {number} submitted_at_ms
       * @property {PullRequestView[]} prs
       */
      /**
       * One row of the flat PR index (RAL-362) -- see
       * `GET /api/pull-requests/index`. Distinct from {@link PullRequestView}:
       * narrower field set (no title/description/base_ref), plus the
       * source squad/task/cell a `PullRequestView` doesn't carry, resolved
       * server-side via `cells.review_branch` the same way `BranchView`'s
       * `source_squad_id`/`source_task_idx`/`source_cell_idx` are. Backs the
       * Tasks tab's review/PR badge lane.
       * @typedef {object} PrIndexRow
       * @property {string} id
       * @property {string} guardian_id
       * @property {string|null} [branch_id]
       * @property {string|null} [branch_alias]
       * @property {string} forge - "github" | "gitlab"
       * @property {string} repo
       * @property {number|null} [pr_number]
       * @property {string|null} [pr_url]
       * @property {string} state - "open" | "merged" | "closed" | "dropped"
       * @property {number} created_at_ms
       * @property {number} updated_at_ms
       * @property {string|null} [source_squad_id]
       * @property {number|null} [source_task_idx]
       * @property {number|null} [source_cell_idx]
       * @property {string|null} [ci_status] - RAL-395: "pending" | "passing" | "failing", from the last standing CI/CD poll. null if never polled.
       * @property {boolean|null} [draft] - RAL-353: see `PullRequestView.draft`.
       */
      /**
       * Live drift check between a PR's remote branch and its owning review
       * worktree (RAL-190) -- see `GET /api/pull-requests/{id}/sync-status`.
       * @typedef {object} PrSyncStatus
       * @property {string|null} [remote_sha]
       * @property {string|null} [local_sha]
       * @property {string|null} [last_pushed_sha]
       * @property {boolean} in_sync
       * @property {boolean} pr_ahead
       * @property {boolean} worktree_ahead
       */
      /**
       * One PR review comment/note (RAL-117) -- see `GET /api/pull-requests/{id}/comments`.
       * @typedef {object} PrCommentItem
       * @property {boolean} actioned
       * @property {string} external_id
       * @property {string} author
       * @property {string} body
       */
      /**
       * One cross-squad task hit in the go-to search overlay (RAL-253).
       * @typedef {object} GotoSearchResult
       * @property {string} squadId
       * @property {number} taskIdx
       * @property {string} taskName
       * @property {string} label
       */
      /**
       * @typedef {object} ChatMessage
       * @property {number} seq
       * @property {string} role
       * @property {string} text
       * @property {number} at_ms
       * @property {string} [author] RAL-379: registered user this feedback is attributed to -- the only identity the board shows. Absent for a "guardian"-role message and for any row predating this field.
       * @property {string} [submitted_by] RAL-379: the resolved authenticated/default requester who actually submitted this message. Audit/provenance only -- never rendered.
       * @property {string} [action_status] RAL-380: this message's completion status ("received"/"done"/"failed"/"superseded"), rendered by RAL-446 as a marker on its chat bubble. Absent for a "guardian"-role message and for any row predating this field.
       */
      /**
       * @typedef {object} ResourceEntry
       * @property {string} squad_id
       * @property {string|null} squad_label
       * @property {number} task_idx
       * @property {string} task_name
       * @property {number} cell_idx
       * @property {string} cell_id
       * @property {number} pid
       * @property {number} cpu_percent
       * @property {number} mem_bytes
       * @property {number|null} gpu_mem_bytes
       */
      /**
       * @typedef {object} CartographerRow
       * @property {number} id
       * @property {number} at_ms
       * @property {string} level
       * @property {string} source
       * @property {string} message
       * @property {string} scope
       * @property {string|null} squad_id
       * @property {string|null} guardian_id
       * @property {string|null} cell_id
       * @property {string|null} [task]
       * @property {Record<string, *>} payload
       */
      /**
       * @typedef {object} DebugEventEntry - one row from a `.../debug-events`
       *   endpoint (RAL-288 Stage 5, generalized to all four terminal-log
       *   contexts in RAL-296) -- `daemon/src/timeline.rs`'s
       *   `SquadTimelineEntry`, scoped to one cell/proof step/guardian branch
       *   resolver/guardian manual-checks run's most recent attempt.
       *   Deliberately a distinct shape from {@link CartographerRow} (no
       *   `id`/`squad_id`/`guardian_id`/`payload`; adds
       *   `log_path`/`log_excerpt`, the latter inlining that entry's own
       *   terminal-log content when it has one).
       * @property {number} at_ms
       * @property {string} level
       * @property {string} source
       * @property {string|null} scope
       * @property {string|null} task
       * @property {string|null} cell_id
       * @property {string} message
       * @property {string|null} log_path
       * @property {string|null} log_excerpt
       */
      /**
       * @typedef {object} EntityRef
       * @property {string} kind
       * @property {number} taskIdx
       * @property {number} cellIdx
       * @property {number} proofIdx
       * @property {string} label
       */
      /**
       * @typedef {object} MachineProviderView
       * @property {string} scheme
       * @property {string} description
       * @property {string} program
       * @property {string[]} args
       * @property {number} protocol_version
       * @property {number} created_at_ms
       * @property {number|null} [last_check_ms] - When reachability was last probed; absent/null means never (RAL-185).
       * @property {boolean|null} [last_check_ok] - Outcome of that probe; absent/null means never probed.
       * @property {string|null} [last_check_note] - The provider's own note, or the failure reason.
       * @property {boolean} [supports_channel] - Provider reuses one process for many commands (RAL-185).
       */
      /**
       * A registered Triage type (RAL-318) -- a `[[cell]]` with `triage = true`
       * classifies into one of these by name instead of naming an explicit
       * `[[review]]`. `"unclassified"` is a built-in that always exists and
       * can never be deregistered.
       * @typedef {object} TriageTypeView
       * @property {string} name
       * @property {string} label
       * @property {string} description
       * @property {number} created_at_ms
       */
      /**
       * The Arbiter's pooled-cell count for one `(project, triage_type)` key
       * (RAL-318) -- cells classified into this type, from any squad, waiting
       * for a threshold or schedule to fire and drain them into one fresh
       * review. `threshold` is null when no count-based trigger is configured
       * for this key yet.
       * @typedef {object} TriagePoolView
       * @property {string} project
       * @property {string} triage_type
       * @property {number} count
       * @property {number|null} threshold
       */
      /**
       * RAL-449: the Triage tab pool table's local "Drain now" confirmation
       * state -- captured straight from a `TriagePoolView` row at the
       * moment the human clicks it, so Confirm always drains exactly what
       * was shown (no server preview round trip; unlike a threshold change
       * there is nothing to compute in advance).
       * @typedef {object} TriagePoolDrainConfirm
       * @property {string} project
       * @property {string} triage_type
       * @property {number} count
       * @property {number|null} threshold
       */
      /**
       * RAL-421: the non-mutating "rough preview" of a proposed pool
       * threshold -- what confirming it would drain *right now*. The daemon
       * computes it from the pool's viable count under its store lock at
       * request time, so it is an estimate, never a reservation: between
       * preview and confirm another submission may pool, or a cron may fire.
       * `clearing` means the proposed change removes the count trigger (no
       * drain); otherwise `full_batches` whole threshold-sized batches would
       * become reviews, `cells_drained` of them in total, and only the
       * `cells_left` sub-threshold remainder would stay pooled.
       * @typedef {object} TriagePoolThresholdPreview
       * @property {string} project - The resolved pool key the confirm would persist under.
       * @property {string} triage_type
       * @property {number|null} proposed_threshold - null when clearing.
       * @property {boolean} clearing
       * @property {number} pooled - Viable pooled-cell count right now (failed cells never count).
       * @property {number} full_batches - Whole threshold-sized batches a confirm would drain now (0 when clearing or below threshold).
       * @property {number} cells_drained - `full_batches * proposed_threshold`.
       * @property {number} cells_left - Sub-threshold remainder that would stay pooled.
       */
      /**
       * One cron-based drain trigger for a `(project, triage_type)` pool key
       * (RAL-318). `anchor_date_ms`/`last_checked_ms` are UTC epoch-ms over
       * the wire -- convert to/from the viewer's local timezone at the UI
       * boundary only.
       * @typedef {object} TriageScheduleView
       * @property {number} id
       * @property {string} project
       * @property {string} triage_type
       * @property {string} cron_expr
       * @property {number} anchor_date_ms
       * @property {number} every_n
       * @property {number} occurrence_count
       * @property {number|null} last_checked_ms
       */
      /**
       * One cell across any squad that has opted into Triage (has at least
       * one resolved type) and has not yet been linked to an actual review
       * -- a row in the Triage tab's candidate list. Present from the
       * moment its type(s) resolve at submit time, whether or not the cell
       * has run yet; disappears once its pool drains into a review.
       * @typedef {object} TriageCandidateView
       * @property {string} squad_id
       * @property {string|null} squad_label
       * @property {number} task_idx
       * @property {string} task_name
       * @property {number} cell_idx
       * @property {string} cell_id
       * @property {string|null} cell_name
       * @property {string} project
       * @property {"scheduled"|"queued"|"failed"} status - "scheduled" (not done yet), "queued" (done and viable, a live candidate for the next pool drain), or "failed" (done but its proof failed -- permanently excluded from ever being swept into an auto-review).
       * @property {string[]} triage_types
       */
      /**
       * @typedef {object} ProjectView
       * @property {string} name
       * @property {string} description
       * @property {string} path
       * @property {string|undefined} clone_url
       * @property {string} vcs
       * @property {number} created_at_ms
       */
      /**
       * RAL-408: a project's raw database-backed review-setting overrides
       * (`GET/POST .../review-settings`'s `settings` field) -- every field
       * `undefined`/`null` means "not configured here, inherit from file
       * config/global".
       * @typedef {object} ProjectReviewSettingsRaw
       * @property {string|null|undefined} default_resolver_agent
       * @property {string|null|undefined} default_resolver_model
       * @property {string|null|undefined} default_machine
       * @property {number|null|undefined} default_maximum_budget_usd
       * @property {string|null|undefined} default_proof_scope
       * @property {boolean|null|undefined} verify_skip_auto_clean
       * @property {boolean|null|undefined} skip_worktrees
       * @property {boolean|null|undefined} skip_base_updates
       * @property {number|null|undefined} base_shift_maximum_rebuilds
       * @property {boolean|null|undefined} match_pr_branch_name
       * @property {boolean|null|undefined} separate_pr_branch
       * @property {boolean|null|undefined} dual_root_pr
       * @property {string|null|undefined} auto_build
       * @property {boolean|null|undefined} auto_submit_pr_stack
       * @property {boolean|null|undefined} auto_fix_pr_errors
       * @property {string|null|undefined} auto_fix_prompt_template
       * @property {boolean|null|undefined} discourage_tests_during_auto_pull_request_fixes
       */
      /**
       * RAL-408: the fully resolved effective review-setting defaults (file
       * config + database) a fresh auto-review would get right now --
       * `GET/POST .../review-settings`'s `effective` field.
       * @typedef {object} EffectiveReviewDefaults
       * @property {string} resolver_agent
       * @property {string|undefined} resolver_model
       * @property {string|undefined} machine
       * @property {number|undefined} maximum_budget_usd
       * @property {string} proof_scope
       * @property {boolean} skip_auto_clean
       * @property {boolean} skip_worktrees
       * @property {boolean} skip_base_updates
       * @property {number} base_shift_maximum_rebuilds
       * @property {boolean} match_pr_branch_name
       * @property {boolean} separate_pr_branch
       * @property {boolean} dual_root_pr
       * @property {string|undefined} auto_build
       * @property {boolean} auto_submit_pr_stack
       * @property {boolean} auto_fix_pr_errors
       * @property {string|undefined} auto_fix_prompt_template
       * @property {boolean} discourage_tests_during_auto_pull_request_fixes
       */
      /**
       * RAL-408: `GET/POST /api/projects/{name}/review-settings`'s response
       * shape.
       * @typedef {object} ProjectReviewSettingsResponse
       * @property {string} project
       * @property {ProjectReviewSettingsRaw} settings
       * @property {EffectiveReviewDefaults} effective
       */
      /**
       * @typedef {object} UserView
       * @property {string} name
       * @property {number} created_at_ms
       * @property {boolean} is_admin
       */
      /**
       * @typedef {object} WatchView
       * @property {string} id
       * @property {string} user_name
       * @property {string} entity_uri
       * @property {string[]} notify_tiers
       * @property {number} created_at_ms
       */
      /**
       * One mailbox message as shown to the acting user (RAL-241/RAL-320),
       * from `GET /api/mailbox/personal/messages`. `read` reflects that
       * user's own drain state (RAL-401's dismiss), not a global property of
       * the message -- another watcher of the same entity has independent
       * read state over the same row.
       * @typedef {object} MailboxMessageView
       * @property {string} id
       * @property {"urgent"|"high"|"normal"} priority
       * @property {string} message
       * @property {string|null} squad_id
       * @property {string|null} task
       * @property {string|null} cell_id
       * @property {number} created_at_ms
       * @property {boolean} read
       * @property {string|null} entity_uri
       * @property {string|null} event_kind
       * @property {string|null} category
       */
      /**
       * A registered fork row (RAL-338): the writable repository a project's
       * review branches are pushed to when the user named by `user` can't
       * push directly to the project's own repository. `user === ""` is the
       * project-wide default row, used when no user-specific row exists.
       * @typedef {object} ForkRecord
       * @property {string} project
       * @property {string} user - "" for the project-wide default row.
       * @property {string} fork_url
       * @property {string} remote_name
       * @property {string} fork_owner - GitHub owner/org login; "" for GitLab, which addresses cross-project MRs by numeric project id instead.
       * @property {number} created_at_ms
       * @property {number} updated_at_ms
       */
      /**
       * One `GET /api/health/project-forks` finding for a registered fork row.
       * @typedef {object} ForkHealthCheck
       * @property {string} project
       * @property {string} user
       * @property {string} name - "fork-user" | "fork-project" | "fork-remote" | "fork-relationship"
       * @property {string} status - "pass" | "warn" | "fail"
       * @property {string} detail
       */
      /**
       * One `GET /api/worktree-retirements` row (RAL-385, states widened by
       * RAL-386 for machine-provider-backed worktrees): a review worktree
       * classified by retirement lifecycle state, or durable history for a
       * worktree already removed. See docs/glossary.md's "worktree retirement"
       * entry for the state vocabulary.
       * @typedef {object} WorktreeRetirementEntry
       * @property {string} guardian_id
       * @property {string} guardian_name
       * @property {string} project_root
       * @property {string} path
       * @property {string} state - "scheduled" | "eligible" | "claimed" | "failed" | "deferred" | "opted_out" | "retired"
       * @property {number} eligible_at_ms - when the worktree became old enough to retire
       * @property {number|null} last_activity_ms - null for retired rows (path columns were cleared)
       * @property {string|null} claim_kind - "claimed" only: the non-terminal claim holding the worktree
       * @property {string|null} claim_owner - "claimed" only
       * @property {string|null} claim_state - "claimed" only
       * @property {string|null} error - "failed"/"deferred"/"opted_out" only: why the attempt was refused, deferred, or declined
       * @property {number|null} last_attempt_ms - "retired"/"failed"/"deferred"/"opted_out" only: when the last attempt ran
       * @property {number|null} retry_at_ms - "deferred" only (RAL-386): the provider's display-only retry hint
       */
      /**
       * `GET /api/whoami` response (RAL-332).
       * @typedef {object} WhoAmI
       * @property {string|null} name
       * @property {boolean} is_admin
       */
      /**
       * @typedef {object} SecretEnvNameView
       * @property {string} name
       * @property {number} created_at_ms
       */
      /**
       * One row of a DB-backed agent profile's environment table (RAL-473).
       * `kind` is `"Set"` (literal `value`) or `"Link"` (`value` names another
       * env var to resolve at cell-run time -- first against this profile's
       * own other entries, then the daemon process environment). The API
       * never returns a resolved `Link` value, only this raw, unresolved
       * shape -- see `GET /api/agent-profiles`.
       * @typedef {object} AgentEnvEntry
       * @property {string} key
       * @property {string} kind - "set" | "link"
       * @property {string} value
       */
      /**
       * `GET /api/agent-profiles/{name}` / one entry of `GET /api/agent-profiles`'s `profiles` array (RAL-473).
       * @typedef {object} AgentProfileView
       * @property {string} name
       * @property {string} backend
       * @property {string|null} executable - only ever set for the "raw" backend; every other backend's command is a global AgentBackendCommandView override.
       * @property {string|null} model
       * @property {AgentEnvEntry[]} env
       * @property {number} created_at_ms
       * @property {number} updated_at_ms
       */
      /**
       * `GET /api/agent-backend-commands`'s per-backend entry (RAL-473) -- a
       * built-in backend's invoked command, overridden globally for every
       * profile selecting that backend.
       * @typedef {object} AgentBackendCommandView
       * @property {string} backend
       * @property {string} command
       * @property {number} updated_at_ms
       */
      /**
       * The 409 `in_use` body `DELETE /api/agent-profiles/{name}` returns when
       * a delete is blocked (RAL-473) -- what's still referencing the profile,
       * for the admin's own judgment call before retrying with `?force=true`.
       * @typedef {object} AgentProfileReferences
       * @property {string[]} squad_ids
       * @property {string[]} guardian_ids
       */
      /**
       * @typedef {object} AvailableAgent
       * @property {string} id
       * @property {string} kind - "builtin" | "profile"
       * @property {string} backend
       */
      /**
       * @typedef {object} AgentOptionsCacheEntry
       * @property {AvailableAgent[]} agents
       * @property {string} defaultAgent - the `id` a review falls back to when it sets no resolver agent of its own (`GET /api/agents`'s `default_agent`).
       */
      /**
       * @typedef {object} SelStateTasks
       * @property {string|null} kind
       * @property {number} taskIdx
       * @property {number} cellIdx
       * @property {number} [proofIdx]
       * @property {string} [proofScope]
       */
      /**
       * @typedef {object} StatusPickerItem
       * @property {string} squadId
       * @property {string} kind
       * @property {number} taskIdx
       * @property {number} cellIdx
       * @property {number} proofIdx
       * @property {string} [proofScope]
       * @property {string} label
       */
      /**
       * @typedef {StatusPickerItem & { key: string, kind: "task"|"cell"|"proof" }} GraphNodeSelectionItem
       */
      /**
       * One `ralphus_core::health_catalog` entry (RAL-416) -- catalog
       * metadata only, no live result. `GET /api/health/catalog`.
       * @typedef {object} HealthCatalogEntry
       * @property {string} id
       * @property {string} label
       * @property {string} section - "core" | "harness" | "machine"
       * @property {string} applicability - "daemon_local" | "remote"
       * @property {string} cost_tier - "free" | "on_demand"
       * @property {string} requirement - "required" | "optional" | "fallback_only"
       * @property {object} probe - `{kind: string, detail?: string}`
       * @property {string} impact
       * @property {string} remediation
       */
      /**
       * One check's cached outcome from the daemon's hourly Free-tier health
       * sweep (RAL-416, `daemon/src/health_sweep.rs`). `GET /api/health/report`
       * / `POST /api/health/report/refresh`.
       * @typedef {object} HealthSweepCheck
       * @property {string} id
       * @property {string} status - "pass" | "warn" | "fail"
       * @property {string} detail
       */
      /**
       * @typedef {object} HealthSweepReport
       * @property {string} machine - synthetic label for this daemon's own host, e.g. "daemon (local)".
       * @property {number|null} checked_at_ms - null if no sweep has completed yet.
       * @property {HealthSweepCheck[]} checks
       */
      /**
       * One named check's outcome for one `[machine.targets.*]` entry
       * (RAL-355 Phase 9 / RAL-416's `id`). `GET /api/machines/targets/health`.
       * @typedef {object} TargetHealthCheck
       * @property {string} name
       * @property {string} status - "pass" | "warn" | "fail"
       * @property {string} detail
       * @property {string} id - RAL-416 `ralphus_core::health_catalog` entry id.
       */
      /**
       * One host a user has a forge personal-access-token configured for
       * (RAL-490). Never carries the token value itself -- `GET
       * /api/users/{user}/forge-tokens` only ever returns this summary shape.
       * @typedef {object} UserForgeTokenSummary
       * @property {string} user
       * @property {string} host
       * @property {number} created_at_ms
       * @property {number} updated_at_ms
       */
      /**
       * @typedef {object} TargetHealthReport
       * @property {string} target
       * @property {string} machine
       * @property {TargetHealthCheck[]} checks
       */
