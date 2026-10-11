---
name: ralphus-submit
description: Use ralphus to submit tickets or tasks as a new squad
---

Given some list of tickets/tasks to work on, create a ralphus Task for each.
Remember: for every ticket, you MUST write the ENTIRE ticket body into its
respective prompt section!

Run `ralphus tutor` first if you haven't already. It explains how to submit to
ralphus, and reading it is critical to a good submission.


## Step 1 - Basic Task Setup
- If I don't specify a Task dependency preference, look for opportunities to
  group similar Tasks together (don't group for the sake of it. Look for actual
  dependencies and prefer parallel runs whenever possible).
- For Proofs, prefer a remediation command over a prompt, so the fix is
  deterministic and cheap, and only fall back to a prompt when no command can
  express the fix.


## Step 2 - Reviews
- If I don't mention how many Reviews I expect to see, assume only one Review.
- For Review tasks, always use cheaper models, not expensive ones.


## Step 3 - Finish Up
- Create a TOML file and show it to me
  - For every ticket, you MUST write then ENTIRE ticket body into its respective prompt section!
- After I say it looks good to submit, go ahead and submit it with ralphus


## Worktree initialization
- If the project's worktrees need gitignored assets restored before a build
  (generated content, copied config), set `initialization_commands` once on
  `[[default]]` (or on the task) rather than telling every prompt to do it.
  It runs once per freshly created worktree, before any cell starts; the
  nearest level wins and `""` disables an inherited value. Cells that share a
  worktree must resolve to the same value. Keep the commands idempotent.
- The board shows the resolved commands read-only on the squad, task and cell
  panes; edit them on a cell.
