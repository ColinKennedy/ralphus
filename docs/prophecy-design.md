# Prophecy — design record

> Design notes from `PROPHECIES.local.md` (2026-09-25)

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
