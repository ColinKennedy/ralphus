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
`RAL-???` placeholder has been replaced at the source, along with the
intro paragraph that still described the superseded milestone / pulse /
member names. That file is git-ignored and lives only in the main
checkout, so it is not carried by this branch; this file remains the
checked-in record of the number and the settled terms.

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
- Injected-waypoint prompt contract: every squad cell/proof system prompt
  (the `waypoint_context` runner wire field, default true; review/guardian
  agent runs set it false and omit the section) must
  state that waypoint information may be injected and is expected; that it
  may describe completed/required/planned/proposed changes and their
  interaction with current work; that the agent must inspect visible/base
  state rather than assume the described change already exists locally;
  and that the agent should respond as applicable, not treat the injection
  as unexpected or as something that blindly overrides its own task. It
  answers with a `RALPHUS_BEARING:` line only when a bearing actually
  appears in its context.
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

## Batch-vs-per-candidate survey classification — resolved

**Decision: one LLM call per candidate. Not batched.** The ticket's own
instruction was "attempt batching only if straightforward, default to
per-candidate calls otherwise," and batching is not straightforward here for a
reason specific to this feature rather than to effort.

Fail-closed classification is per roster entry, and it is load-bearing for
safety: an error, timeout or unparseable reply resolves that entry to
impacted + block. Batching N candidates into one call makes that failure
domain N-wide — one bad call escalates from "one squad held" to "every squad
in the batch held", and the same applies to a single mis-parse in a reply that
has to carry N verdicts. Coarsening the blast radius of the mechanism the
ticket's Risks section calls out as the thing that must not be got wrong is a
bad trade for a cost saving.

The cost pressure batching would relieve is real, and it got sharper once a
terminal agent (`claude-code`, `codex`) became a supported classifier, where
the dominant cost is process spawn and context load rather than tokens. But
the right lever for that is bounding how many classifications run at once,
not widening what a single failure takes down with it — hence
`waypoints::SURVEY_MAX_PER_SWEEP`, which caps dispatch per sweep and defers
the remainder to the next one. Deferring is safe where batching is not: a
deferred candidate keeps the NULL verdict the gate already treats as blocking,
so the cap costs latency and never a missed gate.

Revisit only if per-candidate cost becomes the binding constraint *and*
per-entry fail-closed semantics can be preserved inside a batch — e.g. a
reply format where one unparseable entry fails only itself.

## Watcher notification is per-entity, not per-project

RAL-400's Phase 8 asks for a mailbox message "when a waypoint is created for
a watched project". There is no such thing as watching a project: a watch is
an exact `entity_uri` match (`Store::watchers_for_entity`), and the URI
grammar has no `project:` form. Inventing one is a separate decision about
what project-level watching means for every other event kind, not something
to bolt on here.

What ships instead reaches the same people. A waypoint is created over
specific squads and reviews, and creation notifies the watchers of each of
them; later enrolment notifies through the same path. Someone watching work
in a project hears about any waypoint that touches that work, and hears
nothing about waypoints that do not -- which is the actionable-matching
requirement the ticket opens with, applied to notification.

The message leads with the entry its recipient actually watches and names
the rest only as context, since a watch is an exact match and the recipient
has no stake in the others.

## Halted cells resume, they are not restarted

RAL-400 left this open: when a waypoint closes after halting an in-flight
cell, does the squad auto-resume the same conversation, or is a hard stop
acceptable with a manual `squad restart` afterward? **Confirmed by the user:
auto-resume.**

So a halt keeps the cell's `agent_session_id`, and `run_pending_waypoint_resumes`
re-dispatches it once the gate lifts. Two things have to happen on the way
back in, and both are easy to miss:

- The worktree is rebased onto its upstream first. A cell is held *because*
  the change it must take up does not exist yet; resuming against the tree as
  it stood at halt time means it still cannot see that change.
- The waypoint's guidance rides in on the cell's ghost, saying the change is
  most likely already present after the rebase and that finding nothing to do
  is a legitimate outcome. Without it the agent is re-invoked with no idea it
  was ever held, and sets about re-implementing work its own tree contains.

This is the plan doc's Phase 5 parking design, minus the graceful half
(turn-end checkpointing, live-conversation injection, backend-specific
resume) which stays deferred to v2.

## Phase 11 walkthrough: what was proven against a live daemon

Two registered projects, one of them a monorepo with two independently
edited areas, plus four squads and a review. The point of the setup was the
ticket's actionable-matching requirement: include everything that needs the
waypoint, touch nothing that does not.

A waypoint whose only affected entry was the `auth` squad, with an
outstanding roster entry so its holds were live:

| work | where | outcome |
|---|---|---|
| squad-1 `auth work`, in flight | projA / auth | **halted mid-cell** |
| squad-4 `more auth work`, submitted after | projA / auth | **enrolled at submit, gated before it ran** |
| squad-2 `billing work`, in flight | projA / billing | untouched, no roster state |
| squad-3 `other project work` | projB | untouched, no roster state |

The two untouched squads are the whole claim: a sibling area in the *same*
project and a different project both stayed out, and neither cost a model
call. That the sweep was alive at the time is proven by squad-4, which it
did pick up -- so the zero is selectivity, not inactivity.

The survey then ran live against squad-4 and returned `impacted=false`,
which released it; it resumed and ran to completion. squad-1 stayed held,
correctly: it is an explicit entry, and the classifier never second-guesses
a human declaration.

For the review kind: approval was refused while a `block` entry held it,
with the waypoint named in the error. Setting the entry to advisory released
it -- the next refusal came from the review's own state machine ("it is
collecting"), not the waypoint.

Two defects this surfaced, both fixed in the same change: the approval error
named only the escape hatches and not the resolver's own `RALPHUS_BEARING`
answer, which is the normal way a review gets released; and a hold lifting
was invisible in the waypoint's feed, because the release only notified
watchers and an entry often has none.

Not covered here: a review resolver answering end-to-end, which needs a real
agent on a built review worktree. The store-level release is unit-tested
(`a_review_can_answer_and_release_its_own_approval`), and the equivalent
agent-answers-and-closes loop was verified live on the squad path.

## Still open (explicitly deferred, not this cell's job)

- Advisory stand-down optionality — deferred to Phase 6.
- Mid-turn urgency — explicitly out of scope for the whole ticket.
- Project-scoped watching — see above; needs its own decision, outside RAL-400.
