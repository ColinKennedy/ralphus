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
