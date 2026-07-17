# Overview

A quick tour of the concepts you'll see everywhere else in these docs.

## Runs, tasks, and sessions

You submit a **run** — one TOML file describing a batch of work. A run is
made of one or more **tasks** (a unit of work with its own dependency edges
to other tasks), and each task is made of one or more **sessions** — an
actual agent invocation, either a natural-language `prompt` or a literal
shell `command`. Sessions within a task can depend on each other; tasks
within a run can depend on each other; runs can even depend on other runs.
The scheduler only starts a session once everything it depends on has
finished.

## Verify steps

A task or a session can declare **verify steps**: a shell command that must
exit `0`, or a prompt sent back to an agent whose final answer is parsed for
a pass/fail verdict. Verify steps are what turn "the agent said it's done"
into "the agent's work actually builds/tests/lints clean" — they run for
real and gate whether the task counts as done.

## Scheduling

The daemon resolves the full dependency graph — within a task, across tasks
in a run, and across runs — and schedules whatever is ready to run, up to a
configurable concurrency limit. You get a say in *ordering* among everything
that's currently ready via the [Queue](views/queue.md) tab; ralphus never
lets you violate a dependency, only reprioritize within what's actually
runnable.

## Guardian reviews

Once a task's session finishes on its own branch, an optional **Guardian
review** can stack that branch (and others) into a single rebased review
branch, resolve merge conflicts with an agent, run your declared check gates,
and give you a chat channel to request changes before anything ships. See
[Reviews](views/reviews.md) for the full picture, including how branch order
relates to task dependencies.

## The board

Everything above is visible on one live web board, polling the daemon every
couple of seconds. It has four tabs:

| Tab | What it's for |
|---|---|
| [Tasks](views/tasks.md) | Every run, its tasks and sessions, and the details of any one you select. |
| [Queue](views/queue.md) | Reordering priority among everything currently ready to run. |
| [Reviews](views/reviews.md) | Guardian merge reviews: branch stacking, conflict resolution, check gates, feedback chat. |
| [Resources](views/resources.md) | Live CPU/RAM/GPU usage per running session. |
