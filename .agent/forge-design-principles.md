# Forge design principles

Two cross-cutting rules for anything that talks to GitHub or GitLab
(`daemon/src/forge.rs`, `daemon/src/pr.rs`, `daemon/src/project_forks.rs`,
`daemon/src/review_branch.rs`, and their CLI/MCP surfaces). Both are
existing behavior already, not new scope — this file exists so an agent
reads them as explicit rules *before* touching forge/PR code, instead of
having to reconstruct the intent from scattered comments and tests.

## 1. GitHub/GitLab parity, REST over CLI

**Whatever ralphus lets you do against one forge, it should let you do
against the other.** When adding a new forge-facing capability — a CLI
subcommand, an MCP tool, a daemon API endpoint, a board button — implement
it for both GitHub and GitLab. If true parity isn't possible (one forge
lacks the underlying concept), that's allowed, but it must be a *documented,
deliberate* asymmetry with a stated reason, not a silent gap. The existing
model for this: `docs/fork-workflows.md` documents exactly two such
asymmetries (GitLab always resolves through the fork's own API even for the
root MR; GitHub gets an extra native PR-stack registration call GitLab has
no equivalent for) and explains in each case why the *logical* result still
matches across both forges. A new asymmetry should be written up the same
way, in the same place (or here, if it's more general than one workflow) —
not left for the next person to discover by diffing behavior.

**Forge calls go through each provider's REST API directly, never through
the `gh`/`glab` CLIs.** `daemon/src/forge.rs`'s `ForgeClient` calls GitHub's
and GitLab's REST APIs straight with `ureq`, matching the daemon's existing
habit (`chat_client.rs`) of talking to provider HTTP APIs directly rather
than shelling out. The **only** sanctioned use of `gh`/`glab` anywhere in
ralphus is `resolve_cli_token` in `daemon/src/forge.rs` — a best-effort
fallback that asks the CLI for a token already cached from a prior
interactive login, used only when `RALPHUS_GITHUB_TOKEN`/
`RALPHUS_GITLAB_TOKEN` (or `[forge].token_env`) isn't set. Do not add a new
code path that shells out to `gh`/`glab` for an actual PR/MR/stack
operation — add it to `ForgeClient` as a REST call instead. See
`docs/glossary.md`'s `forge` entry and `docs/dependencies.md`'s `gh / glab`
section for the existing statement of this rule.

## 2. A submitted PR/MR must always fold into the review's stack

A Guardian review's branches are rebased into a **linear stack**
(`base_branch` ← branch 1 ← branch 2 ← ...) specifically so they can be
submitted as a matching linear chain of PRs/MRs — each one based on the
previous branch's PR, not on the shared upstream. That's the entire point
of stacking the branches in the first place: a reviewer sees one focused
diff per PR, in order, instead of N PRs that all diff against upstream and
overlap each other. **A PR that ends up based on upstream instead of on its
predecessor's PR branch — "loose" instead of chained — defeats that purpose
even if the PR itself is otherwise correct.**

The rule this implies for any code that submits or updates a PR/MR:

- Submitting a PR for a branch must always attempt to chain its base onto
  the nearest preceding *enabled* branch's already-open PR (or onto the
  review's `base_branch` if it's first in the stack). This must be seeded
  from **every** already-open PR recorded for the review, not just the ones
  in the current call/request — see `docs/daemon-api.md`'s
  `POST /api/guardians/{id}/pull-requests` section: submitting one branch at
  a time must chain exactly as correctly as submitting the whole stack in
  one call.
- A base-branch change, a reorder, or a branch being dropped/disabled must
  re-resync every downstream PR's base so the chain stays intact —
  never leave a PR pointed at a base that no longer reflects the stack's
  current order.
- If the stack's root PR merges (or is closed) out from under the rest of
  the chain, the next branch must be promoted/re-chained rather than left
  orphaned against upstream — see `docs/fork-workflows.md`'s
  reconcile-first promotion behavior for the cross-repository case.
- On GitHub, register/grow the native PR-stack grouping alongside the
  base-chain when possible — but the base-chain itself, not the native
  stack registration, is what must never be allowed to break; GitLab has no
  native-stack equivalent and must still get this right through the
  base-chain alone.

This is already the implemented contract — see `daemon/src/pr.rs`'s module
doc and `submit_stack_for_guardian`/`resync_pr_bases`/`check_pr_merges`.

**Regression-test requirement:** any change that touches PR/MR submission,
base resync, reordering, promotion, or the forge clients those call
(`daemon/src/pr.rs`, `daemon/src/forge.rs`) must ship with a regression test
that proves the stack is preserved or correctly rebuilt — not just that "a
PR gets created" or "a PR gets updated" in isolation. Extend the existing
test families rather than inventing a new shape for this:
`stack_base_for_chains_onto_nearest_preceding_alias`,
`stack_base_for_seeded_from_a_prior_call_still_chains`, the
`decide_stack_action_*` family, `per_branch_submission_still_registers_the_native_github_stack`,
`repoint_stacked_prs_*`, `reconstruct_forge_chain_*`/`reconstruct_forge_stack_*`,
and the `check_pr_merges_*` family (all in `daemon/src/pr.rs`'s test module).
A PR-submission change with no test in one of these shapes should be
treated as incomplete, the same way a schema change with no validator test
would be.

## 3. GitHub job-level ambiguity vs GitLab's full trace

**Incident (RAL-408/RAL-110, 2026-09-13):** an auto-fix dispatch against
GitHub PR #106 was told `'Docs (screenshot coverage lint)' failed`, ran the
screenshot-coverage lint locally, saw it pass, and declared the branch clean
— while GitHub's CI kept failing on that same commit. The job actually
bundles **two** unrelated checks as separate steps: the screenshot lint, and
a `cli-reference.md` freshness check (`uv run ralphus-docs-helpmap --check`,
RAL-110). GitHub's Checks API reports only the *job's* name and conclusion to
[`ForgeClient::check_pr_ci_status`] — nothing about which of its several
independently-tracked steps actually broke — and the check-run's own
`output.text`/`output.summary` (meant to carry a failure detail) is empty for
most CI setups, this repo's included. An agent told only "job X failed" with
no log has no way to know the job is two checks wearing one name, so it
verifies whichever half matches the job's title and calls it done.

**The fix:** [`FailedCheck::failing_step`] (`daemon/src/forge.rs`) is a
forge-agnostic field — any [`FailedCheck`] can carry the specific
step/command that actually failed, distinct from the parent job/check's own
`name`. [`ForgeClient::github_failing_step`] populates it for GitHub, on
demand, by fetching the Actions API's per-job `steps` array (only when a
caller is about to hand a failure to an agent — `ci_watch::run_pr_fix` — not
on every routine 2-minute standing-poll tick, since that would mean an extra
forge call per failing check on every poll for a cost the routine poll never
needed). `ci_watch::describe_failing_check` surfaces it in the auto-fix
prompt as an explicit "the specific check that failed is: 'X' — do not
assume a differently-named step under the same job is the problem" warning.

**Why GitLab doesn't get the same treatment today, and isn't assumed safe
either:** GitLab CI has no structural equivalent to a GitHub Actions
composite job's individually-tracked steps — a GitLab job is one pass/fail
unit around a `script:` list, with no per-line conclusion the API exposes.
What GitLab *does* already give this module, and GitHub does not, is the
job's complete raw trace (`ForgeClient::gitlab_job_trace`, fetched into
`FailedCheck::log_text` for every failing job unconditionally, not gated
behind an extra on-demand call) — the literal output of whichever script
line actually failed is already in what the agent reads today, which is
exactly the piece GitHub's `output.text` almost never provides. That
materially reduces the odds of the same failure mode: the ambiguity in the
GitHub incident came from having *no signal at all* below the job name, not
from a job merely running more than one command.

That said, this is **not** the same guarantee `failing_step` gives — a
GitLab job with several script lines chained by `;` instead of `&&`, or one
that doesn't stop at the first failure, could still bury the real failing
command mid-trace where a skimming agent misses it. If a GitLab-side
incident of this shape ever surfaces, the fix belongs in one of two places,
in this order of preference: (1) if GitLab's API ever exposes finer job-step
granularity, populate `failing_step` from it the same way GitHub does,
keeping the field's forge-agnostic contract; failing that, (2) parse
`gitlab_job_trace`'s output for the actual failing command (e.g. the last
non-zero-exit shell invocation) and surface that explicitly, the same
"don't assume, here's specifically what broke" way `describe_failing_check`
already does for GitHub. Do not build either speculatively — GitLab's
existing full-trace behavior already covers the common case; treat this
paragraph as the place to start only once a real GitLab incident of this
shape is confirmed, the same way the GitHub fix above followed a confirmed
incident rather than a hypothetical one.
