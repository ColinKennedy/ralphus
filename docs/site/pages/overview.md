# Overview

A quick tour of the concepts you'll see everywhere else in these docs.

## Squads, tasks, and cells

You submit a **squad** — one TOML file describing a batch of work. A squad is
made of one or more **tasks** (a unit of work with its own dependency edges
to other tasks), and each task is made of one or more **cells** — an
actual agent invocation, either a natural-language `prompt` or a literal
shell `command`. Cells within a task can depend on each other; tasks
within a squad can depend on each other; squads can even depend on other
squads. The scheduler only starts a cell once everything it depends on has
finished.

## Proof steps

A task or a cell can declare **proof steps**: a shell command that must
exit `0`, or a prompt sent back to an agent whose final answer is parsed for
a pass/fail verdict. Proof steps are what turn "the agent said it's done"
into "the agent's work actually builds/tests/lints clean" — they run for
real and gate whether the task counts as done.

## Scheduling

The daemon resolves the full dependency graph — within a task, across tasks
in a squad, and across squads — and schedules whatever is ready to run, up to
a configurable concurrency limit. You get a say in *ordering* among
everything that's currently ready via the [Queue](views/queue.md) tab;
ralphus never lets you violate a dependency, only reprioritize within what's
actually runnable. A cell rostered on an open, blocking
[waypoint](views/waypoints.md) is a third kind of gate alongside dependency
edges and concurrency: it shows as running rather than stuck, and resumes on
its own once the waypoint's survey clears or the waypoint closes — see
[Cross-squad waypoints](#cross-squad-waypoints) below.

## Guardian reviews

Once a task's cell finishes on its own branch, an optional **Guardian
review** can stack that branch (and others) into a single rebased review
branch, resolve merge conflicts with an agent, prepare optional manual checks,
and give you a per-branch feedback thread to request changes before anything
ships. See [Reviews](views/reviews.md) for the full picture, including how
branch order relates to task dependencies.

## Cross-squad waypoints

Where a Guardian review stacks branches *within* one squad's tasks, a
**waypoint** (RAL-400) tracks impact *across* squads and reviews that
otherwise have no dependency edge between them: a named prompt plus a
**roster** of squads and/or reviews an LLM **survey** pass classifies as
impacted (or not), each in **block** or **advisory** mode. A rostered squad
whose survey comes back blocking halts (rather than fails) until it either
receives a **bearing** — a durable, append-only note of what changed
elsewhere and why it matters here — or the waypoint closes. See
[Waypoints](views/waypoints.md) for the full picture, including how a
waypoint interacts with a review's restack.

## The board

Everything above is visible on one live web board, polling the daemon every
couple of seconds. It has nine tabs:

| Tab | What it's for |
|---|---|
| [Tasks](views/tasks.md) | Every squad, its tasks and cells, and the details of any one you select. |
| [Queue](views/queue.md) | Reordering priority among everything currently ready to run. |
| [Reviews](views/reviews.md) | Guardian merge reviews: branch stacking, conflict resolution, prepared manual checks, per-branch feedback. |
| [Waypoints](views/waypoints.md) | Cross-squad coordination join points: roster, survey verdicts, delivery feed, bearings. |
| [Resources](views/resources.md) | Live CPU/RAM/GPU usage per running cell. |
| [Logs](views/cartographer.md) | The unified Cartographer event log, filterable and drillable. |
| [Projects](views/projects.md) | Registered git repositories a task's cell `cwd` can materialize a worktree under. |
| [Machines](views/machines.md) | Registered providers that remote tasks, cells, and proof steps run on. |
| [Users](views/users.md) | Placeholder identities a request can attribute itself to (not authentication). |
| [Secrets](views/secrets.md) | Env-var names whose resolved values get scrubbed from pane text and terminal logs. |
