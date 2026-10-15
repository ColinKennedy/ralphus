# CI deferral

A stack of PRs/MRs does not need every member to run the whole, slow CI suite.
Ralphus can mark the members that may skip it with one label,
`ralphus:defer-full-ci`, and your CI configuration decides what "skip" means.
Ralphus never skips a test itself: it only keeps the label right, and it keeps
CI in step with the label.

A PR/MR **without** the label runs everything. A missing label, a typo in the
setting, or a repository that ignores the label all cost CI minutes, never
coverage.

## The setting

`ci_deferral` chooses which members of a review's stack run the full suite.
For a stack of PRs 1 (bottom) to 4 (top):

| Mode | Full CI on | Label on |
|---|---|---|
| `every` (default) | 1, 2, 3, 4 | nothing |
| `top` | 4 | 1, 2, 3 |
| `bottom+top` | 1, 4 | 2, 3 |

"Bottom" is the PR/MR that targets the base branch; "top" is the last one. A
stack of one is both, so it always runs everything, and with two members
`bottom+top` runs both.

It resolves, most specific first: the review's own value, then the project
default, then `[review] default_ci_deferral` in the daemon's global config
(`~/.config/ralphus/config.toml`, or `$RALPHUS_CONFIG_HOME/config.toml`), then
`every`.

| Where | How |
|---|---|
| A review | `ralphus review settings <review> --ci-deferral bottom+top`, `ci_deferral = "bottom+top"` under `[[review]]`, the API, or the board's review setup window. An empty value clears it back to the project default. |
| A project | `ralphus project review-settings set <project> --ci-deferral top`, the API, or the board's project Review Settings window. |
| The daemon | `default_ci_deferral = "top"` under `[review]` in the global config. |

## What ralphus does when the setting changes

Ralphus works out which PRs/MRs of the review should carry the label, reads the
current labels, and touches only the ones that differ. For each of those:

1. It records the CI runs in flight on the PR/MR's head commit. They were
   started under the old scope, so they are out of date.
2. It adds or removes the label.
3. Only once that succeeded, it cancels exactly the runs recorded in step 1.
   A run the label change itself starts is newer and is never cancelled.
4. GitLab does not start a pipeline when a label changes, so ralphus asks it for
   a new merge request pipeline. GitHub does it on its own when the workflow
   listens for label changes (see below).

A PR/MR whose label is already right is not touched, and its CI is left alone.

### What triggers a re-label

The labels follow the stack, not only the setting:

- changing the setting on a review, or changing a project default for the
  reviews that inherit it;
- submitting a PR/MR (it is labelled against the setting at that moment, and
  the previous top stops being exempt);
- reordering the review's branches, and enabling or disabling a branch;
- moving a branch to another review.

Anything else that changes the stack is caught by the daemon's periodic review
check, which compares the review's stack (its mode and its open PRs/MRs in
order) with the one the labels were last set for and re-labels only if they
differ. That covers a PR/MR merged, closed or dropped on the forge, a bulk
unlink, and any other path. The comparison reads only local state, so a review
whose stack did not change costs no forge call. The first check of a review
after the daemon starts only records its stack.

Two things this does not do:

- A PR/MR that has left the stack (its branch disabled, or the PR closed or
  dropped) keeps the label it last had, because ralphus no longer manages it.
  It is labelled again from scratch if it rejoins the stack.
- Adding a branch to a review changes nothing until that branch has a PR/MR,
  since only open PRs/MRs are in the stack. The submission that creates it
  re-labels the stack.

Steps 1, 3 and 4 are best-effort: a token that cannot list or cancel runs, or a
project with no merge request pipelines, is logged and does not undo the label
change. Labels you add by hand are overwritten the next time the review syncs.

## Making a repository honour the label

Nothing reads this label until you tell your CI to. Keep the fast jobs
(format, lint, build, docs checks) unconditional and put the condition only on
the slow ones.

### GitHub Actions

Two changes. Listen for label changes, so that a changed label re-runs CI, and
skip the slow jobs when the label is present:

```yaml
on:
  pull_request:
    types: [opened, synchronize, reopened, labeled, unlabeled]

concurrency:
  group: ci-${{ github.ref }}
  cancel-in-progress: true

jobs:
  lint:                      # fast: no condition, always runs
    runs-on: ubuntu-latest
    steps: [...]

  full-tests:                # slow: skipped on a labelled PR
    if: ${{ !contains(github.event.pull_request.labels.*.name, 'ralphus:defer-full-ci') }}
    runs-on: ubuntu-latest
    steps: [...]
```

For a single slow step inside a job, put the same expression in that step's
`if:`. Keep job names unchanged so required-check settings keep matching.
Ralphus's own `.github/workflows/ci.yml` is a worked example.

#### Gate on the live labels

GitHub's create-PR API has no labels field, so ralphus labels a new PR in a
call right after creating it, and the `opened` event's payload
(`github.event.pull_request.labels`) is taken before that call and never
carries the label. A condition written against the payload alone would start
the slow jobs on `opened` and only cancel them when `labeled` arrives. Add a
small gate job that reads the labels from the API and make the slow jobs
depend on it:

```yaml
jobs:
  defer-gate:
    runs-on: ubuntu-latest
    permissions:
      pull-requests: read
    outputs:
      deferred: ${{ steps.labels.outputs.deferred }}
    steps:
      - id: labels
        env:
          GH_TOKEN: ${{ github.token }}
          PAYLOAD_DEFERRED: ${{ contains(github.event.pull_request.labels.*.name, 'ralphus:defer-full-ci') }}
        run: |
          deferred="$PAYLOAD_DEFERRED"
          if names=$(gh api "repos/${{ github.repository }}/issues/${{ github.event.pull_request.number }}/labels" --jq '.[].name'); then
            if printf '%s\n' "$names" | grep -qx 'ralphus:defer-full-ci'; then deferred=true; else deferred=false; fi
          fi
          echo "deferred=$deferred" >> "$GITHUB_OUTPUT"

  full-tests:
    needs: defer-gate
    if: ${{ needs.defer-gate.outputs.deferred != 'true' }}
    runs-on: ubuntu-latest
    steps: [...]
```

Ralphus sets the label as soon as it creates each PR/MR (GitLab takes it in
the create call itself), but it decides "top" or "middle" from the stack as
submitted so far, so a final reconcile still corrects it. The gate narrows the
window the label cannot close; it does not remove it, and the API read adds a
few seconds before the slow jobs start.

Things to know:

- Without `labeled` and `unlabeled`, a label change re-runs nothing.
- Any label change on the PR re-runs CI, not only this one. The concurrency
  group above cancels the run it replaces.
- A job skipped by `if:` counts as passing for required checks, so a skipped
  slow job looks green. A short job that writes "slow jobs deferred" to the run
  summary makes that visible.
- Ralphus cancelling stale runs needs a token that may write to Actions, in
  addition to the pull-request/issue permission that changes labels.

### GitLab CI/CD

Run merge request pipelines, and skip the slow jobs when the label is present.
`$CI_MERGE_REQUEST_LABELS` is a comma-separated string available only in merge
request pipelines:

```yaml
workflow:
  rules:
    - if: $CI_PIPELINE_SOURCE == "merge_request_event"
    - if: $CI_COMMIT_BRANCH == $CI_DEFAULT_BRANCH

lint:                        # fast: no condition, always runs
  script: [...]

full-tests:                  # slow: skipped on a labelled MR
  rules:
    - if: '$CI_PIPELINE_SOURCE == "merge_request_event" && $CI_MERGE_REQUEST_LABELS =~ /(^|,)ralphus:defer-full-ci(,|$)/'
      when: never
    - when: on_success
  script: [...]
```

Things to know:

- GitLab does not start a pipeline for a label change. Ralphus requests a new
  merge request pipeline itself (step 4 above), which needs a token with the
  `api` scope and a project that runs merge request pipelines. Without those it
  logs a warning and the change takes effect on the next push.
- Rules are read when the pipeline is created, so the label has to be on the
  MR before the pipeline starts. That is why ralphus creates the new pipeline
  after the label change rather than relying on the one a push started.
- `rules:` evaluates the label, but a job left out by `rules:` does not exist in
  that pipeline. Check that nothing `needs:` it, or mark the dependency
  `optional: true`.

## What the label does not do

- It does not replace a test strategy. Tests that run on the top of the stack
  cover the whole accumulated stack, not each intermediate state, which is why
  this suits stacks that land as a unit.
- It does not make a skipped job report success on its own terms; see the
  required-check note above.

## Auto-fix on a deferred stack

A bug that an earlier branch introduces first fails the tests on a later PR/MR,
usually the top one. When `auto_fix_pr_errors` dispatches a fix for any PR/MR
above the bottom of the stack, the prompt names the branches below it, says
that their code is built and tested here too (and, when they are deferred, that
their own PRs/MRs skipped the full suite), and tells the agent to trace the
failure to wherever the code lives instead of searching only this branch's own
commits.
