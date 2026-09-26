# Prophecy — design record

*Design source: [`PROPHECIES.local.md`](../PROPHECIES.local.md)*

## 0. The idea in one paragraph

When an agent learns something the diff can't show — why a path was taken, what
was left behind in a rebase, a hazard noticed but not fixed — it should be able
to say so, durably, at the moment it learns it. Those statements accumulate
across a task's attempts and its review, and surface in the PR so the reasoning
arrives with the code instead of evaporating with the cell.

## 1. Verdict — five decisions

1. **The transport is a marker, not an MCP server.** The daemon reads a cell's
   stderr line by line and already attributes each event to the owning cell
   from its own `RunnerSpec`. A marker-based write needs no token, no daemon
   URL, and no endpoint the agent can reach — and one cell cannot forge
   another's attribution. See §4, §5.
2. **A prophecy is append-only; a ghost is merge-on-write.** They answer
   different questions for different readers on different clocks. Keep both;
   do not widen ghost into this. See §3.
3. **No new parent entity.** Addressing already exists (`EntityUri`) and
   Cartographer already stamps squad/task/cell/guardian per row. One
   correlation key across the squad↔review↔PR seam buys the through line for a
   fraction of the blast radius. See §7.
4. **No new capability for the agent.** Not a daemon URL, not a token. The
   URL is a hardcoded default and the token is a readable file, so withholding
   them protects nothing locally — and exporting them would ship the daemon's
   bearer token to every remote machine. See §5, §6.
5. **The prose goes in the PR body; git gets only a join key.** A one-line
   trailer naming the authoring cell, not the prophecy set. See §8.

## 11. Open questions

### 11.1 Is `kind` a closed enum?

`AGENTS.md` requires asking rather than guessing on new-field validation.
Suggestion is a closed set — `discovery`, `decision`, `hazard`, `deferred` —
because open strings become forty synonyms for "note" within a month and
nothing is filterable. But a closed set rejects a kind wanted later. Real
trade; needs a decision before phase 1's schema.

### 11.2 Does a prophecy survive its squad's deletion?

Ghosts cascade-delete. If a prophecy does too, deleting a squad silently strips
the reasoning out of an already-open PR. Leaning **survives**, which means a
nullable squad reference and an orphan-retention policy.

### 11.3 Does a prophecy feed forward into prompts, or only outward to humans?

Feeding it back into dependent cells makes it a better ghost. It also
reintroduces the context-cost problem ghost's 4000-char cap exists to prevent.
Cheapest resolution: outward only in v1, revisit after phase 5.

### 11.4 Do we want the pre-existing credential exposure ticketed?

Independent of prophecy: a cell's agent can read `daemon.token` and assert any
`X-Ralphus-User`, so it can already cancel squads and merge reviews (§5).
RAL-252 (verified login) and RAL-225 (container mode) are the two existing
threads that would close it. Reasonable to accept for a single-user dev tool —
but better as a written decision than an unexamined default. Note
`SECURITY_AUDIT.local.md` does not currently mention it.
