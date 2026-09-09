      // ---- Shared type definitions (JSDoc-only; see docs/daemon-api.md for the
      // authoritative wire shapes). Checked via `npm run typecheck` (tsc --checkJs
      // over the extracted <script> body) — see tsconfig.board.json. ----
      /**
       * @typedef {object} ProofView
       * @property {string|null} id
       * @property {string} kind - "command" | "prompt" | "brain" | "approval"
       * @property {string} state - "pending" | "running" | "done" | "failed" | "cancelled"
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
       * @property {{[key: string]: string}} [env_overrides] - Persistent environment-variable overrides set directly on this cell (hierarchical env overrides, extending RAL-150); merged on top of the owning task's/squad's when the cell's own subprocess is spawned. Empty/absent for the vast majority of cells.
       * @property {{[key: string]: string}} [proof_env_overrides] - Persistent environment-variable overrides applied only to this cell's own proof steps, merged on top of `env_overrides` (and its ancestors).
       * @property {number|null} [started_at_ms]
       * @property {number|null} [finished_at_ms]
       * @property {boolean} [env_out_of_date] - RAL-271: cosmetic "out of date" badge — true once this cell's own `env_overrides` has been edited since the cell last ran/retried or had its status explicitly set. No behavioral effect.
       * @property {number|null} [detached_at_ms] - RAL-288: when this cell was cleanly stopped for a real interactive agent session to take over, or absent/null while it isn't detached. State still reads `running` while this is set — the cell is paused for a human, not stuck. Clears automatically on the cell's next dispatch (a restart, or resume-automation).
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
       * (`cwd`), its shared project root, and its read-only display upstream.
       * @typedef {object} CellPathInfo
       * @property {number} task_idx
       * @property {number} cell_idx
       * @property {string|null} worktree
       * @property {string|null} project
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
       * @typedef {object} SquadView
       * @property {string} id
       * @property {string|null} label
       * @property {string} state
       * @property {number} created_at_ms
       * @property {number|null} [started_at_ms]
       * @property {number|null} [finished_at_ms]
       * @property {TaskView[]} tasks
       * @property {{id: string, name: string, status: string}[]} [reviews]
       * @property {{[key: string]: string}} [env_overrides] - Persistent environment-variable overrides (RAL-150); applied to every cell/proof subprocess this squad spawns from now on, until unset. Empty/absent for the vast majority of squads.
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
       * @property {string} [merge_status] - "pending" | "in_progress" | "done" | "proof_pending" | "conflict_resolved" | "failed" | ...
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
       * One squad or review a user has chosen to hide from their own view
       * (RAL-328/RAL-331). `squad_id`/`guardian_id` are mutually exclusive,
       * set per `kind`.
       * @typedef {object} HiddenItem
       * @property {string} kind - "squad" | "review"
       * @property {string|null} squad_id
       * @property {string|null} guardian_id
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
       * @typedef {object} GuardianView
       * @property {string} id
       * @property {string} name
       * @property {string} status
       * @property {string} [base_branch]
       * @property {string[]} [projects]
       * @property {string[]} [squash_projects]
       * @property {GuardianBranch[]} branches
       * @property {string} [squad_id]
       * @property {string} [git_root]
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
       * @property {boolean} [readable_review_branch] - RAL-378: whether this review's combined worktree branch is named readably rather than as the internal guardian/<id>/review ref. False for reviews created before readable naming landed.
       * @property {string|null} [review_branch_name] - RAL-378: the sticky readable name claimed for this review's combined worktree branch, derived from its name at the first combined build.
       * @property {boolean|null} [auto_submit_pr_stack] - RAL-317: this review's own override for whether the PR stack is auto-submitted/grown as each branch reaches a terminal merge state, or null to inherit the project/global default. Stamped from the project's effective value at review creation.
       * @property {boolean} [effective_auto_submit_pr_stack] - RAL-317: auto_submit_pr_stack resolved against the project/global default -- what the per-branch auto-submit trigger actually gates on.
       * @property {string} [origin] - RAL-318: provenance of this review -- "explicit" (an authored [[review]] block, or any other pre-existing creation path -- the default/normal case) or "arbiter" (created automatically by the Arbiter/Triage subsystem when a pooled cell count threshold or cron schedule fired).
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
       * One `GET /api/worktree-retirements` row (RAL-385): a review worktree
       * classified by retirement lifecycle state, or durable history for a
       * worktree already removed. See docs/glossary.md's "worktree retirement"
       * entry for the state vocabulary.
       * @typedef {object} WorktreeRetirementEntry
       * @property {string} guardian_id
       * @property {string} guardian_name
       * @property {string} project_root
       * @property {string} path
       * @property {string} state - "scheduled" | "eligible" | "claimed" | "failed" | "retired"
       * @property {number} eligible_at_ms - when the worktree became old enough to retire
       * @property {number|null} last_activity_ms - null for retired rows (path columns were cleared)
       * @property {string|null} claim_kind - "claimed" only: the non-terminal claim holding the worktree
       * @property {string|null} claim_owner - "claimed" only
       * @property {string|null} claim_state - "claimed" only
       * @property {string|null} error - "failed" only: why git refused the last removal attempt
       * @property {number|null} last_attempt_ms - "retired"/"failed" only: when the last attempt ran
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

