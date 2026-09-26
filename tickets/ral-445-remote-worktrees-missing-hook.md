# RAL-445 does not reach remote worktrees

**Status:** open
**Found by:** prophecy subsystem design (`docs/prophecy-design.md`, §6.3 / §13), 2026-09-25
**Component:** daemon

## Problem

`sync_coauthor_hook` (`daemon/src/git_hooks.rs`) is only called from the
local `git worktree add` path — `execute_worktree_plan` in
`daemon/src/worktrees.rs:1204`. The remote provisioning path never calls it.

A remote worktree therefore receives no `prepare-commit-msg` hook at all, so
every commit made on a remote machine is silently missing its
`Co-authored-by:` attribution. This is a correctness gap in the RAL-445
co-author hook feature, not a new subsystem's bug — it predates and is
independent of prophecy, which would inherit the same hole for its own
`Ralphus-Cell:` trailer if that trailer were added before this is fixed.

## Suggested fix

Port `daemon/src/git_hooks.rs` from operating on `&Path` to operating on
`&Workspace`, and call it from the remote provisioning path the same way
`execute_worktree_plan` calls it locally. `Workspace` already exists
(`daemon/src/workspace.rs`, RAL-185 Phase 3c) and dispatches git/file
operations to the owning machine provider when remote (`ws.git()`,
`ws.write_file()`), which is the same pattern `guardian_merge.rs` already uses
for remote-safe git operations.

## Not being fixed here

This ticket records the gap; it is not fixing it. See
`docs/prophecy-design.md` §13 for the context that surfaced it.
