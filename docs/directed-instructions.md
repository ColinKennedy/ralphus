# Directed instructions (`/ralphus ...` in a prompt cell)

Status: Phases 1 and 2 built (marker scanner, per-cell storage, cell-start
pre-pass in `daemon/src/directed_instructions.rs`; the `pr` consumer in
`directed_instructions.rs`, `pr.rs` and `exhibits.rs`). Later phases extend
this page rather than any other plan document.

A **directed instruction** is a part of a prompt cell's `prompt` addressed to
ralphus rather than to the cell's agent, e.g. `/ralphus make sure the PR has
two GIFs` or `... also Ralphus, please test this against the dev server`.
Vocabulary: [`glossary.md`](glossary.md); marker grammar:
[`special-syntax.md`](special-syntax.md).

## Behaviour

Before a prompt cell's agent starts, a one-shot pre-pass splits the prompt
into:

1. **original** -- exactly what the user wrote (the cell's stored `prompt`,
   never modified),
2. **work prompt** -- the original minus the directed parts; this is what the
   cell's agent receives,
3. **directed instructions** -- zero or more `{target, text}` rows stored
   durably per cell, travelling cell -> review -> PR description / live action.

No new TOML syntax and no task-file schema change. **No task-file field is
added in Phase 0**, and no `core/src/validate.rs` change is made.

## Settled decisions

- Runs **once, at cell start, as a preprocessor**, cached by the hash of the
  original prompt. No mid-run triggers; it never influences cell execution.
- Marker and prefilter: see `special-syntax.md`. A prompt without the
  substring `ralphus` (any case) skips the pre-pass, so most cells pay nothing.
- Targets (closed registry):
  - `pr` -- GitHub PRs and GitLab MRs alike.
  - `live_action` -- additive guidance only: attached to the cell's live
    handoff if the cell posts one, dropped if it posts none. Never creates a
    live action.
- A compound instruction may fan out to several targets. A single instruction
  of uncertain target gets exactly one (the pre-pass picks); if truly unclear
  it stays in the work prompt.
- Any pre-pass failure or malformed or unknown-target output falls back to the
  original prompt, recorded as an informational Cartographer row (not a
  failure mailbox message). The pre-pass never fails the cell.
- Out of scope: `proof` (must not be changeable this way), review ordering,
  `auto_action`, `later_cells`, and `notify` (needs mid-run triggers; only a
  reserved registry slot).

## Resolved questions

### Where the work prompt is spliced in

`daemon/src/runner.rs::RunnerSpec::from_row` copies `row.prompt` into
`RunnerSpec.prompt` (`prompt: row.prompt.clone()`). Its only production
caller is `run_cell_worker` in `daemon/src/scheduler.rs`
(`let mut spec = RunnerSpec::from_row(squad_id, row);`), which then adjusts
`spec.system_prompt`, `spec.agent` and `spec.executable` from the resolved
agent selection. The splice point is right after that call: the pre-pass runs
there and, when it produced a work prompt, overwrites `spec.prompt`.
`row.prompt` itself is untouched, so the stored original, the later handoff
recording (`row.prompt.is_some()` after the cell finishes) and the board all
keep seeing the user's text. Nothing in `core` or the runner crate changes.

### The one-shot agent call to reuse

There is no dedicated "one-shot" API: the PR writer
(`daemon/src/pr.rs`, `pr_writer_spec` then `runner.run(&spec)`) and the
resolver passes build a prompt-only `RunnerSpec` (no system prompt, no
timeout/budget fields, `proof: false`) and call `Runner::run`
(`daemon/src/runner.rs`, the `Runner` trait), reading the reply from
`RunnerResult.summary`. The pre-pass does the same through the scheduler's
existing `Runner`, so backend-specific behaviour stays inside the concrete
Claude Code / Codex / Pi / native implementations and no daemon-side lookup
table is added. Unlike the PR writer, which uses the review's resolved
resolver agent (`guardian_merge::ResolvedResolverAgent`), the pre-pass spec
takes the cell's own `selection.backend`, `selection.executable`, model and
machine, so it runs on the cell's assigned agent. It sets
`closing_turn_prompt: None` (no live-handoff question) and must not carry the
cell's system prompt. Because it runs through the normal runner path, it is a
process-spawning behaviour and needs a `proc_counts/tags.toml` entry in
Phase 1.

### How `pr_guidance` is rendered and how a cell-sourced entry behaves

`GuidanceEntry { text, paths }` (`daemon/src/pr_guidance.rs`) layers are
collected in `pr.rs::pr_guidance_note`: the review's
`GuardianView.effective_pr_guidance` (global config, then review or project
setting), then `.ralphus.toml` file entries for the PR's changed paths.
`pr_guidance::guidance_note(entries, changed)` keeps entries whose `paths` is
empty or matches a changed file and renders them as one block, "Project
guidance for PR descriptions (additional instructions, applied in order; the
PR template's headings and structure still take precedence)", spliced into the
PR-writer prompt as `{guidance_note}`.

A cell-sourced `pr` instruction should be another source of `GuidanceEntry`
with **empty `paths`**, so it always participates for the PR it belongs to,
and appended after the existing layers in `pr_guidance_note`. Open points
for Phase 2:

- The per-entry cap `MAX_PR_GUIDANCE_CHARS` (2000) applies to settings and
  files; a cell-sourced entry should be truncated with `truncate_with_marker`.
- Scoping: a review's PR for branch N should only receive instructions from
  cells that belong to that branch's task, not every cell in the review. This
  is how the stack position is mapped to its member cells; it must be checked
  against `branch_changed_paths` and the branch-to-task link before building
  the collector.
- The rendering wording says "Project guidance"; cell-sourced text is
  user-authored but relayed through an LLM, so the consuming prompt must label
  it as data, not as instructions that can override the PR template.

### Cache invalidation on `cell edit`

`Store::edit_cell_fields` (`daemon/src/store.rs`) is the single store choke
point for cell edits and tracks `prompt_touched`; the HTTP handler reaches it
through `server.rs` (with `cell_edit_is_noop` guarding identical resubmits). A
prompt edit changes `row.prompt`, so a cache keyed by the hash of the
original prompt misses on its own. **The prompt hash alone is enough; no
explicit invalidation hook is added.** A stale row for an old hash is simply
replaced wholesale (the pattern of `replace_cell_live_handoffs` in
`live_actions.rs`) on the next pre-pass.

## Phase 1 implementation notes

- Tables `cell_directed_instructions` (one row per cell: prompt hash, work
  prompt, status `extracted|unchanged|fallback`) and
  `cell_directed_instruction_rows` (target, text, source `marker|semantic`).
  A `fallback` result is cached too, so a retry is stable.
- `source` is `marker` when the instruction text is contained in a paragraph
  that `scan_markers` matched, otherwise `semantic` (the reply contract is
  just `{work_prompt, instructions:[{target,text}]}`).
- The hook (`apply_prepass`) runs in `run_cell_worker` just before handoff
  resolution; the pre-pass call reuses the cell's own spec (agent, model,
  machine) with no system prompt or closing turn.
- No `proc_counts/tags.toml` entry: the tests use a scripted `Runner` and
  spawn no process; the real pre-pass goes through the existing runner path.

## Phase 2 (`pr` consumer) implementation notes

- `collect_review_pr_instructions_conn` returns the `pr` rows of **every**
  member cell of a review (membership as in `review_member_cells_conn`), not
  just the latest per task, each with its source-cell URI. `GuardianView`
  carries them as `pr_instructions` (not serialized).
- Branch mapping: a cell's `review_branch` is its task branch name, equal to
  the review branch's `branch`. A PR gets the instructions of its own
  branch's cells plus those of cells that recorded no branch.
- `pr_guidance_entry` turns them into one always-applying `GuidanceEntry` in
  the existing `pr_guidance_note` channel: the union, identical texts listed
  once with all their cells, the whole block labelled as user-authored data
  that cannot change the template, format or rules. Direct contradictions are
  flagged in the text with a WARNING naming both sides and their cells.
- Contradiction detection is a keyword heuristic (one side negated, at least
  60% overlap of content words). It misses paraphrased or semantic
  contradictions and can flag a pair that merely shares vocabulary; it only
  adds a warning, nothing is dropped.
- Instructions naming a screenshot, GIF or video (optionally with a count,
  "two GIFs") become exhibit requests under the new `directed` origin, which
  sorts between `handoff` and `recipe` when the PR slot cap cuts. They are
  attached when the PR is created, if exhibits are enabled for the review.
  Other instructions stay guidance only.
- Submission and stack logic are untouched; regression tests submit a
  two-branch stack with a `pr` instruction on both GitHub and GitLab mocks.

## Open questions

- Opt-out: no `directed_instructions = false` field is added. If one is
  wanted, how it should be validated (closed enum, boolean, or deliberately
  unvalidated) must be decided by the user before any
  `core/src/validate.rs` work.
- Should a contradiction the heuristic cannot see (a semantic opposite) be
  judged by the agent that writes the PR, or by an extra pre-pass call?

## Risks

- Misrouting is silent: the board display (Phase 4) is the mitigation, plus a
  bias toward leaving ambiguous text in the work prompt.
- A wrong strip makes the worker miss real instructions: keep the original,
  fall back to it on any pre-pass error, cache by hash so a retry is stable.
- Extra latency/cost per cell is bounded by the prefilter and the cache.
- Instruction text is untrusted input to the PR writer and live-action
  prompts: consuming prompts treat it as data.
