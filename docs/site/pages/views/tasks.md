# Tasks

The Tasks tab is the default view and the one you'll live in most: every run
you've submitted, its tasks and sessions, and the full detail of whichever
one you've selected.

## The board at a glance

![Tasks tab showing four runs in different states, with the running run's task/session tree expanded](../screenshots/tasks-overview.png)

The **left sidebar** lists every run, newest first, filterable by status and
searchable by id/label:

- The colored dot + label is the run's **state** — `running`, `done`,
  `failed`, or `queued` (held back with `hold=true`, waiting to be
  activated). A `queued` run shows a `▶ Run` button right there in the
  sidebar.
- Below the label, the small meta line repeats the state as text and (for
  anything past `pending`/`queued`) offers a logs shortcut.
- Right-click any run for rename / retry / restart / cancel / delete, or to
  manually override its status.

The **main pane** shows the selected run's task tree: each task's name,
state, and declared task-level verify steps, with its sessions nested below.
In the screenshot above, `add-dark-mode-toggle` is `running` — one session
(`wire-theme-toggle`) already finished and its `fmt` verify step passed;
the second session (`persist-theme-choice`) is still in flight and depends
on the first.

## Selecting a session

![The same board with a specific session selected, showing its details pane](../screenshots/tasks-session-detail.png)

Click any session to open its **details pane**: the resolved agent and
model, token/cost counters, the prompt or command it ran, its own dependency
(`persist-theme-choice` waits on `wire-theme-toggle`, shown above), and any
session-level verify steps. This is where you'd go to read exactly what an
agent was asked to do and what it reported back — the same pane a
`command`-kind session shows its captured output in.

## Where task state comes from

A task's state rolls up from its sessions and verify steps — it only counts
as `done` once every session has finished and every verify step has passed.
A `failed` session (like `extract-token-parser` in the `refactor-auth-
middleware` run) fails its owning task, and any task-level verify step
attached to it is shown alongside the failure so you can see exactly which
check caught it.
