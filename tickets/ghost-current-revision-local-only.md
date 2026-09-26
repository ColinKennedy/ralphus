# ghost::current_revision is local-only

**Status:** open (smaller, optional)
**Found by:** prophecy subsystem design (`docs/prophecy-design.md`, §6.3 / §13), 2026-09-25
**Component:** daemon

## Problem

`ghost::current_revision` (`daemon/src/ghost.rs:202`) shells out to the local
`git` binary directly. On a remote `cwd` (a cell running through a machine
provider), this returns `None` and the ghost's revision marker silently
vanishes instead of reflecting the remote worktree's actual revision.

## Suggested fix

Route through `ws.git()` (`daemon/src/workspace.rs`) instead of the local
`git` free function, matching how `guardian_merge.rs` already dispatches git
operations through the owning machine provider when remote.

## Not being fixed here

This ticket records the gap; it is not fixing it. See
`docs/prophecy-design.md` §13 for the context that surfaced it.
