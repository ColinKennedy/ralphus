# Why ralphus?

ralphus orchestrates autonomous agent tasks: you describe work as a TOML task
file, the system runs it with whatever model you point it at — a cloud model
like Claude, or a fully local Ollama model — verifies the result actually
did what it claimed, and shows the whole thing on a live web board.

## The problem it solves

ralphus is a from-scratch successor to an earlier internal project
(`claudectl`) whose *task graph* — dependencies between steps, verification
gates, review workflows — worked well, but whose **agent provisioning**
routinely broke: sessions would silently hang, model backends would drift out
of sync with what the task file declared, and a submitted task could sit
forever in a "queued" state without anyone noticing.

ralphus rebuilds the part that broke, behind a model-agnostic runner that is
exercised end-to-end by both cloud and local models — so the same task file
that works against Claude also works, unmodified, against a local Ollama
model. Concretely, that means:

- **A daemon that owns all state.** One process (`ralphus-daemon`) is the
  single source of truth over SQLite; the CLI and the web board are just
  clients of its HTTP API. Nothing can drift because nothing else holds
  state.
- **Submissions run immediately, not silently.** A submitted task lands in a
  schedulable state right away — no more tasks stuck in a "queued forever"
  limbo unless you explicitly ask to hold one back.
- **Verification is part of the pipeline, not an afterthought.** A task can
  declare shell-command or prompt-based verify steps that actually run and
  actually gate whether the work is considered done.
- **Review is a first-class workflow.** Guardian reviews stack each task's
  branch into a rebased review branch, resolve conflicts with an agent, run
  check gates, and give you a feedback channel to steer changes before they
  ship — see [Reviews](views/reviews.md).

## Who this is for

Anyone who wants to hand off a batch of well-scoped coding tasks to an agent,
walk away, and come back to a board that shows exactly what ran, what passed
verification, and what needs a human look — without babysitting a terminal
per task.

> **TODO:** comparison to similar tools (e.g. other agent-orchestration or
> CI-for-agents projects). Filled in once we've done a fair side-by-side.

## Where to go next

- [Overview](overview.md) for the core concepts (tasks, sessions, verify
  steps, dependency scheduling, Guardian reviews) in one page.
- [Installation](installation.md) to get a daemon and board running locally.
- The [Tasks](views/tasks.md), [Queue](views/queue.md),
  [Reviews](views/reviews.md), and [Resources](views/resources.md) pages for
  a tour of the board itself, screenshots included.
