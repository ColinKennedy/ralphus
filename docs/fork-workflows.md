# Fork-based stacked PR workflows (RAL-338)

Contributors often have read access to a canonical repository but push
permission only to their own fork. This document covers how ralphus routes a
review's stacked PRs/MRs through a registered fork when the acting user can't
push directly to the project's own repository, and what to expect from the
resulting topology.

See the [glossary](glossary.md#reviews) for the **fork**/**parent**/
**promotion** terms this document uses throughout, and
[`daemon-api.md`](daemon-api.md#fork-registration-ral-338) for the wire
shapes of the HTTP endpoints referenced below.

## Setup

Ralphus only *registers* an existing fork — it never creates one through a
forge API. Create the fork yourself (the forge's own "Fork" button, or
`gh repo fork`/`glab repo fork`), then register it:

```
ralphus project fork add <project> --url <fork-clone-url> [--user <name>] [--remote-name <name>] [--owner <owner>]
```

- Omit `--user` to register the **project-wide default row** (`user = ""`),
  used whenever no user-specific row exists for whoever is submitting.
- `--remote-name` defaults to `fork` for the default row, else
  `fork-<user>`. It names the local git remote ralphus creates/updates
  automatically the first time it pushes through this fork — you don't need
  to run `git remote add` yourself.
- `--owner` is the GitHub owner/org login the fork lives under, needed to
  build the cross-repository PR's `owner:branch` head. Auto-derived from a
  GitHub-shaped `--url` when omitted; GitLab addresses cross-project MRs by
  numeric project id instead, so it stays unset there.

`ralphus project fork list|set|remove` cover the rest of the CRUD surface
(also reachable over HTTP and MCP — see `daemon-api.md`). The `user` field is
a lookup detail, not an authorization boundary: any admin can create, edit,
list, or remove any fork row, including one naming a user later removed from
the user registry — those rows deliberately survive (see
[Unregistered users](#unregistered-users) below).

## Topology

For a fork-enabled review, every branch's git ref is pushed to and fetched
from the **fork**. Only where the PR/MR is *filed* differs:

| Branch | Push/fetch remote | PR/MR filed on | PR/MR base |
|---|---|---|---|
| Lowest enabled, unmerged branch (the **root**) | fork | parent repository, logically | parent's base branch |
| Every later branch | fork | fork | preceding branch's own alias |

"Filed on" is the logical destination; the forge API call itself is
asymmetric between GitLab and GitHub:

- **GitLab** always calls the *fork's* API (`POST
  /projects/{fork_id}/merge_requests`), adding a numeric `target_project_id`
  pointing at the parent only for the root MR. A GitLab root's `repo` is
  therefore the fork's own encoded path, not the parent's — this is why
  ralphus never uses `repo` alone to decide "is this the root" (see
  [Promotion](#promotion)).
- **GitHub** calls the *parent's* API (`POST /repos/{parent}/pulls`) for the
  root, with `head = "<fork-owner>:<alias>"`, and the fork's API for every
  other branch.

A project with **no registered fork** is completely unaffected — routing
stays byte-identical to before this feature existed.

### Alias uniqueness

A PR branch alias is scoped to whichever repository it's physically pushed
to — the fork, for every branch in fork mode, including the root's (even
though the root's *PR* is filed elsewhere, its git ref still lives on the
fork).

### Combined-worktree PRs and multi-project reviews

A combined-worktree PR (`branch_id: null` in a submission request) always
represents the whole stack, so it routes cross-repository to the parent —
same as any other root, since it's the stack's only worktree either way. A
review whose branches span multiple projects, where only some of them use a
registered fork, is out of scope for v1 — declare a `machine` on a fork-mode
branch and it fails loudly at submit time rather than silently pushing to
the wrong place.

## Submitting

Nothing changes about the CLI/API surface you already use — `ralphus review
pr submit`/`POST /api/guardians/{id}/pull-requests` resolve fork routing
automatically once a fork applies. The acting user is resolved the same way
as everywhere else in the daemon: the `X-Ralphus-User` request header, else
`[daemon].default_user` for pollers and other daemon-internal callers.

Before the first ref is ever pushed, a one-time **pre-flight** confirms the
fork is forge-recognized as related to the parent — comparing both projects'
fork-network roots, not merely whether one is a descendant of the other, so
sibling forks and inverted registrations are caught too:

- **same network** (direct fork) or **indirectly related** (a fork of a
  fork, or a sibling) — proceeds; the indirect case logs a warning, since
  cross-repository behavior is only proven for a direct relationship.
- **no relationship** — blocks the whole submission. Pass
  `--allow-unlinked-fork` (CLI) / `allow_unlinked_fork: true` (API body) to
  downgrade this to a loud, logged warning instead.
- **not visible** (403/404 on either project) — proceeds with a warning. A
  private or unreadable project looks identical to a nonexistent one over
  these APIs, so this is never treated as proof of "no relationship."
- **cross-instance** (different forge hosts/kinds) — always blocks; there is
  no cross-repository PR path between two different forge instances,
  regardless of any real relationship.

## Promotion

The stack drains from the bottom. When the root PR merges, the next enabled
branch's PR — until now fork-internal — must become the new cross-repository
root, since a same-repository base PATCH (the ordinary mechanism a
non-fork/same-repo resync uses to skip a merged branch) can't move a PR
across repositories.

Ralphus reconciles this automatically, the next time it polls for merges:
it reads the successor's live state first (**reconcile-first**) and does
nothing if it's already correctly filed against the parent — GitHub/GitLab
auto-retargeting and merge-train behavior are unproven for this
cross-repository topology, so ralphus never fights whatever the forge may
have already done. Otherwise it:

1. Creates a fresh PR/MR for the successor's own alias against the parent
   (the same route calculation submission uses, since the successor is now
   the root).
2. Closes the old fork-internal PR/MR and leaves a pointer comment linking
   to the replacement.
3. Records the closed row's `superseded_by` pointing at the new row's id —
   the closed PR is never deleted, so its discussion stays visible in
   `GET /api/guardians/{id}/pull-request-stacks` history.

At most one branch is promoted per observation, even if multiple branches
merged between polls — ralphus walks forward from the just-merged root's
position to the first branch that's still genuinely open (skipping any that
also merged in the same batch) and promotes exactly that one.

"Is this PR the root" is decided by comparing its recorded base against the
guardian's own base branch, not by comparing `repo` — a GitLab root's `repo`
is the fork's own path (see [Topology](#topology) above), so a `repo`-based
check would never fire for GitLab at all.

GitHub's native PR-stack registration (`POST /repos/{repo}/stacks`) is
repository-scoped and can't mix a parent PR number with fork PR numbers, so
it only ever registers the contiguous fork-local subset — the root's PR at
the parent is tracked solely through its own `PullRequestView` row, never
through the native stack object. GitLab has no equivalent stack-registration
concept; its target-branch chain is what makes the stack, and remains
mandatory regardless.

## Failure remediation

- **"fork does not appear to be a forge-registered fork of the parent"** —
  create it through the forge's own "Fork" action, or (if it already is one
  and this is a false negative) ask a forge admin to link it retroactively.
  Pass `--allow-unlinked-fork` to submit anyway once you're confident the
  relationship exists but isn't forge-visible.
- **"fork has no registered fork_owner"** (GitHub only) — set `--owner` when
  registering the fork (`ralphus project fork set <project> --owner
  <login>`).
- **"the fork and its parent are on different forge instances"** — there is
  no fix; a cross-repository PR only exists within one forge instance/kind.
- A fork row naming a **user who was later removed** from the user registry
  is never silently dropped (see [Unregistered users](#unregistered-users))
  — remove or reassign it explicitly with `ralphus project fork remove`/
  `set`.

### Unregistered users

Fork rows are keyed by `(project, user)` but don't cascade on user deletion:
a row registered for a user who's since been removed from the registry stays
exactly as it was, still resolvable if that name is ever used again, and
flagged (not hidden) by `ralphus check health` and the board's fork
management UI so it doesn't silently orphan.

## Health checks

`ralphus check health` (and `GET /api/health/project-forks`) runs an
advisory pass over every registered fork row, reusing the exact same
relationship classification submission's pre-flight uses rather than a
second implementation: missing/mismatched local git remotes, unreachable or
unrecognized forks, unregistered users, indirect-but-valid chains, definite
missing relationships, and cross-instance impossibility. A `fail` here is
informational only — it does not block anything by itself; submission's own
pre-flight is what's authoritative.

## Permissions

Fork registration mutations (`POST`/`PATCH`/`DELETE`) are admin-gated the
same way project registration is — see `daemon-api.md`'s note on
`POST /api/projects`. Reads (`GET .../forks`, the health check) are open to
every caller, matching how project registrations themselves are exposed.

## Out of scope (v1)

- Ralphus creating a fork through a forge API — it only registers existing
  ones.
- Remote-machine provisioning for a fork clone or separate fork credentials.
  A fork-mode branch that declares a `machine` fails loudly instead of
  silently pushing to the wrong place.
- Separate forge tokens for the parent and the fork (the daemon's normal
  per-forge-kind token resolution applies to both).
- Migrating an already-active non-fork review into fork mode after the fact.
- A review whose branches span multiple projects where only some of them use
  a registered fork.
