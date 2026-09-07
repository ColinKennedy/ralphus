# Tasks

The Tasks tab is a flat, searchable table of **every task across every
squad** — one row per task, regardless of which squad it lives in, with
usage, review, and PR state joined in. Use it to see what's outstanding
board-wide without having to click through each squad's tree on the
[Squads tab](../squads/).

## The task table

![Tasks tab showing a flat table of every task across every squad](../screenshots/tasks-overview.png)

Each row is one task, sorted by default so the most recently started work
sits on top:

- The **squad chip** (colored dot + label) names the owning squad; hover to
  highlight the squad's other rows, or click to jump to that squad on the
  Squads tab.
- The **task name** and a summary of its cells, its own state (`running`,
  `done`, `failed`, …), and its token/cost usage.
- A **review/PR lane** shows review state and any submitted pull-request for
  the task, and a **watch star** reflects whether you're watching the task
  (directly or via its squad).

The toolbar above the table filters rows:

- **filter by task name** narrows the list with a case-insensitive substring
  (tip: just start typing — the list narrows as you type).
- **show hidden** includes tasks belonging to a squad you've hidden on the
  Squads tab (hiding is squad-level only).
- **needs me** shows only rows you're directly responsible for right now:
  watched or watched-squad tasks that are `failed`, awaiting your review
  approval, or approved with no PR submitted yet.
- **status filters** toggle which task states are visible.

## Expanding a task

Rows are collapsed to one line by default. Click **Expand all** (or a task's
expand arrow) to reveal the task's cells beneath it — each cell row shows its
name, state, and agent/model, and pops its full details in the right-hand
pane when selected. **Collapse all** folds the table back down.

## Where the rows come from

The table is built from the same squad data the Squads tab renders — the
Tasks tab is a read-only, board-wide projection of it for triage. Nothing you
do here changes a task's status; it only changes your own view (which rows
are visible) and your watch on a task.