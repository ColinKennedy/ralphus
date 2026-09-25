# RAL-400 waypoints — Phase 0 decisions

Cross-squad waypoints (originally designed under the working name
"milestone" in the git-ignored `MILESTONE_PLAN.local.md`) let a human or
agent declare a named, open/closed join point that a set of squads/reviews
must respect. This file is the durable, checked-in record of the Phase 0
("Decisions & naming") acceptance criteria for RAL-400 — later phase cells
(schema, store, survey, gating, HTTP/CLI/MCP, board, docs) implement against
it instead of the local-only plan doc, which cannot be edited from every
worktree this multi-phase ticket runs in.

## Terminology (final, per RAL-400's own ticket text)

| Term | Was (plan doc) | Meaning |
|---|---|---|
| **waypoint** | milestone | A named, open/closed join point: `prompt` (required), `agent`/`model` (the survey classifier), `allow_advisory` (default off), and a `roster`. |
| **roster** | members | The list of entities a waypoint tracks as impacted. v1 kinds: **review** and **squad**. Cell/task-level roster entries are deferred to v2. |
| **roster entry** | member | One item in a roster — a single review or squad reference. |
| **bearing** | (new) | A durable, append-only guidance item a waypoint publishes for an impacted roster entry's agent: an optional git commit id + message summary, a concise change description, and an optional entity link. The waypoint's counterpart to a **ghost** (`docs/glossary.md`), but ongoing guidance rather than a one-time handoff note. |
| **survey** | pulse / classification pass | The LLM call that classifies whether a review/squad is actually impacted by a waypoint, and at what mode. |

These are final by direct instruction — do not re-litigate naming in a later
phase. `docs/glossary.md` now carries a `## Waypoints (RAL-400)` section
with these same five terms; this file is the fuller design record, the
glossary entries are the short taken-word reference.

## RAL number

**RAL-400** (this ticket's own number). `MILESTONE_PLAN.local.md`'s
`RAL-???` placeholder cannot be fixed at the source — that file is
git-ignored and lives only in the main checkout, outside every worktree a
phase cell runs in, and this Phase 0 cell was explicitly told not to edit
it. A grep of this worktree's tracked files for `RAL-\?\?\?` returns no
matches, so there is nothing checked-in left to fix — this file is the
substitute durable record naming the real number for every later phase to
cite.

## Already-settled decisions (confirmed verbatim, implement as-is)

Recorded during the RAL-400 ticket interview; restated here so later phases
have one place to check instead of re-deriving them:

- `prompt` is required, no silent default.
- `model` requiredness is agent-aware: `claude-code`/`codex` require it,
  `pi` doesn't, and agent profiles forbid it — mirrors existing per-agent
  requiredness rules elsewhere in the schema.
- `allow_advisory` defaults **off**.
- Two-phase roster validation: offline, same-submission-file placeholder
  match at validate time; daemon-live existing-entity check at submit time.
- No `project` field on a waypoint — inferred by hopping through its
  roster's review/squad projects, the same "projects are inferred, not
  declared" principle the plan doc used for members.
- Zero roster entries is rejected **at creation** (vacuous terminality is
  not a valid starting state — a waypoint must name at least one entry to
  exist at all).
- A failed review does not close or fail its waypoint — only a waypoint's
  own terminal states do that. Review failure and waypoint closure are
  unrelated lifecycles.
- Classification (survey) is fail-closed by default: an error, timeout, or
  no-verdict result must resolve to impacted + block, never a silent drop
  or fail-open.
- Per-roster-entry mode (advisory vs. block) is survey-decided and
  human-overridable.
- There is no anchor concept — a waypoint's authority comes entirely from
  its `prompt` + roster + survey outcome, not from any additional pinned
  commit/ref/version marker.
- v1 roster entry kinds are **review and squad** (squads promoted to v1,
  revising the plan doc's reviews-only scope). Cell/task-level roster
  entries are v2.
- Scenario-1 gating: no separate "attach a not-yet-formed review once it
  spawns" mechanism. A pre-review squad an open waypoint affects is gated
  directly as a squad-kind roster entry. A review is gated/notified
  directly only when (a) it's an explicit roster entry from waypoint
  creation, or (b) the squad that produced it has already completed and
  moved into review phase. These two cases never overlap.
- Advisory-mode meaning: for a **review**, advisory = deliver as feedback
  without holding up approval, block = hold approval until the waypoint
  closes. For a **squad**, advisory = let it keep running while the note is
  delivered for awareness (no cell halted), block = halt its in-flight
  cell(s) until the waypoint closes.
- Injected-waypoint prompt contract: every cell/proof system prompt must
  state that waypoint information may be injected and is expected; that it
  may describe completed/required/planned/proposed changes and their
  interaction with current work; that the agent must inspect visible/base
  state rather than assume the described change already exists locally;
  and that the agent should respond as applicable, not treat the injection
  as unexpected or as something that blindly overrides its own task.
- Bearing contract: waypoints keep an append-only list of actual change
  bearings, including relevant git commits and commit-message summaries
  when available, so a later agent can narrow investigation before
  resorting to a broad diff.

## EntityUri decision (RAL-155)

**Yes** — a waypoint gets an `EntityUri`. Ref chips, mailbox messages, and
bearing/delivery rows all want a stable single-string handle, exactly the
existing `Guardian { guardian_id }` case already establishes for reviews.

Grammar (extends `daemon/src/entity_uri.rs`'s existing colon-separated,
kind-prefixed scheme): add a fifth variant alongside `Squad`/`Task`/`Cell`/
`Proof`/`Guardian`:

```
waypoint:<waypoint_id>
```

- `Waypoint { waypoint_id: String }`, `Display` renders `waypoint:<id>`,
  parsed the same way `guardian:<id>` is today.
- `covers()`: a leaf, like `Proof`/`Guardian` — it covers only itself. A
  waypoint's roster entries (reviews/squads) are separate top-level
  entities in their own right, not children nested under the waypoint the
  way task/cell/proof nest under a squad, so the parent-cascades-to-children
  rule doesn't apply here.
- `waypoint_id` should follow the same id-prefix convention as `squad-…`/
  `guardian-…` (e.g. `waypoint-…`) for consistency; the concrete generator
  is Phase 1/2 schema+store work.

This is a separate concern from the **roster-reference sentinel grammar**
below, which is how a `[[waypoint]].roster` entry *names* an existing or
same-file-placeholder squad/review inside TOML — the EntityUri is the
runtime, store-side addressing form used after the waypoint exists.

## Roster-reference sentinel grammar (for Phase 1's schema work)

Mirrors the existing review-reference precedent in `core/src/schema.rs`
(`REVIEW_REF_PREFIX = "<<review:"`, `REVIEW_LINK_PREFIX =
"ralphus:new-review/"`, `parse_cell_review_sentinel`). A `[[waypoint]].roster`
entry needs three forms, decided here so Phase 1 doesn't re-derive them:

1. **Existing review** — reuse `<<review:<id>>>` verbatim; no new grammar
   needed.
2. **Existing squad** — new sentinel form `<<squad:<id>>>`, structurally
   identical to `<<review:<id>>>` (a `SQUAD_REF_PREFIX = "<<squad:"`
   constant and a `parse_cell_squad_sentinel`-shaped parser mirroring
   `parse_cell_review_sentinel`).
3. **Same-file placeholder for the squad this submission itself creates** —
   new sentinel `<<ralphus:new-squad>>`, **no `<key>`** (unlike
   `ralphus:new-review/<key>`) because one submission file always produces
   exactly one squad, so there is nothing to disambiguate between multiple
   same-file candidates the way multiple `[[review]]` blocks require.

A bare, unwrapped id remains invalid for a roster entry, matching the
existing review-reference validation rule (`core/src/validate.rs`'s
`check_review`) — always require the `<<...>>` wrapper.

## Actionable-notification matching model

This is the model the ticket calls out as needing to be "settled before
store work starts." It must (a) include every squad/review that needs to
act on a waypoint, (b) exclude work with no possible or needed action,
(c) be derivable the same way for roster entries and survey candidates,
(d) explain its repository-wide fallback, (e) support waypoints spanning
several areas, and (f) never delegate boundary discovery to an LLM.

**Decision: reuse RAL-346's existing subproject-resolution machinery
(`daemon/src/triage.rs`'s `SubprojectResolution`) as the partition,
extended with a squad/review-level aggregation this ticket's later phases
implement.** The ticket explicitly asked to assess whether existing
metadata offers a better partition before inventing a new one — RAL-346
already solves the structurally identical problem ("which corner of a
monorepo does this work touch, without asking an LLM at match time") for
Triage pooling, and its three-state model already encodes the correct
fail-safe defaults:

- `NotApplicable` — project isn't monorepo-keyed (no `[monorepo]
  subprojects` in `.ralphus.toml`) — the permanent state for a
  single-project repo.
- `Unresolved` — project IS a monorepo, but nothing has classified this
  cell's subproject(s) yet (Arbiter inference pending or found no match).
- `Resolved { subprojects, inferred }` — a concrete, non-empty set, either
  Arbiter-inferred or seeded directly from `CellDef.subprojects`.

Note this model is read-only at match time: a `Resolved` row may have been
populated earlier by the Arbiter's async LLM-assisted inference (RAL-346),
but the waypoint matching step below never itself calls an LLM — it only
consumes already-recorded/config-derived rows. That is what keeps boundary
discovery out of the survey's hands.

### Aggregation: cell scope → squad/review scope

Neither `SubprojectResolution` nor `pool_keys_for_cell` exist above
cell granularity today. Phase 1/2 needs a new aggregate, computed per
project a squad/review touches:

```
fn aggregate_scope(cells: &[SubprojectResolution]) -> Scope {
    // Scope::RepoWide | Scope::Areas(BTreeSet<String>)
    if cells.iter().any(|c| matches!(c, NotApplicable | Unresolved)) {
        Scope::RepoWide   // conservative: can't safely rule anything out
    } else {
        Scope::Areas(union of every Resolved{subprojects} set)
    }
}
```

- **Squad scope** = `aggregate_scope` over every cell in the squad, per
  project the squad's tasks touch.
- **Review scope** = `aggregate_scope` over the union of cells in every
  task feeding that review's branches, per project.

`RepoWide` is contagious under union — one `NotApplicable`/`Unresolved`
cell pulls the whole squad/review to `RepoWide` for that project, since a
false negative (silently excluding work that needed the waypoint) is
unacceptable while a false positive (one extra survey call) only costs
money. This directly satisfies "intentional repository-wide fallback":
every single-project repo is always `RepoWide` (there is no subproject
metadata to narrow with), and a monorepo squad/review with any
not-yet-classified cell degrades to `RepoWide` rather than being narrowed
on an incomplete picture.

### Waypoint scope

A waypoint's own scope, per project, is the union of every **explicit
roster entry's** aggregate scope (same function, applied to reviews and
squads identically — this is what "derivable consistently for roster
entries and survey candidates" means: one function, two call sites). Union
with `RepoWide` is `RepoWide`. This directly supports multi-area waypoints:
a waypoint whose roster spans `{core}` and `{utils}` gets scope
`Areas({core, utils})`.

### Candidate matching (survey population — Phase 2)

For each open review / non-terminal squad not already an explicit roster
entry, in a project the waypoint's scope touches:

- project mismatch → excluded (project identity, via the existing
  `pool_key_for_path`/registered-project lookup, is the first, cheap
  filter — this is also how "nested repositories" are handled: a nested
  repo is its own registered `project`, so the project-identity filter
  already partitions across repository boundaries before subproject
  partitioning ever runs within one project).
- waypoint's scope for that project is `RepoWide` → included (every
  squad/review in that project is a candidate).
- waypoint's scope is `Areas(S)`:
  - candidate's own scope is `RepoWide` → included (can't rule it out).
  - candidate's own scope is `Areas(T)` → included iff `S ∩ T ≠ ∅`;
    excluded only in this one case, where non-overlap between two
    confidently-`Resolved` sets is affirmatively established.

Explicit roster entries bypass this filter entirely (a human/agent
declared them directly at waypoint-creation time — that declaration is
never second-guessed); the filter only narrows the *discovery* population
survey calls would otherwise have to cover exhaustively.

### What Phase 1–3 still need to build

This file settles the model, not the code. Concretely still open for later
phases: the `Scope` type itself, the squad/review aggregation functions
above (naming left to the implementing phase), and wiring the candidate
filter into the survey's pre-classification step. None of that requires a
new design decision — it's a direct implementation of the model above.

## Glossary

`docs/glossary.md` gained a `## Waypoints (RAL-400)` section with the five
terms above, added as part of this Phase 0 cell (the ticket's own
unchecked bullet literally asks for it, even though it's annotated
"(Phase 10)" — doing it now is low-risk, idempotent, and removes the
ambiguity of leaving it to a later cell that may not re-derive the exact
wording this file settles). A later Phase 10 documentation sweep can still
expand or adjust these entries; it will find them already present rather
than missing.

## Still open (explicitly deferred, not this cell's job)

- Batch-vs-per-candidate survey classification cost/failure-isolation
  tradeoff — deferred to Phase 2.
- Advisory stand-down optionality — deferred to Phase 6.
- Mid-turn urgency — explicitly out of scope for the whole ticket.
