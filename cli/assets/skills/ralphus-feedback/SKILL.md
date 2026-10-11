---
name: ralphus-feedback
description: Send requested changes to the right branch worktrees of one or more ralphus Reviews with `ralphus review feedback`
---

I'm going to give you an ID or URL to one or more ralphus Reviews and ask for
changes. I may name exact branch worktrees, or just ask for a change and you
find the right worktree. I might give a list of changes that go to different
worktrees; find them all.

Once you've found a worktree for every suggestion, use `ralphus review feedback`
to send each message to the correct worktree.

## How to do it
- Inspect the Review(s) I named (for example `ralphus review show <id>`; run
  `ralphus --help` or `ralphus review --help` if you need the exact commands) to
  list each branch and what it changed. A branch is addressed by the selector
  `<guardian-id>#<branch>`.
- Match every requested change to the branch whose changes it concerns. If I
  named a branch, use that one. If a change could belong to more than one
  branch, read the branches' diffs before choosing.
- Send one message per change:

  `ralphus review feedback [--author <name>] <guardian-id>#<branch> "<change request>"`

  Write each message so it stands alone: say what to change and why, since the
  receiving agent sees only that message.
- Never edit a Review's worktrees directly. All code changes go through
  `ralphus review feedback`; downstream branches restack automatically.
- Comments left on a forge PR/MR are pulled in with
  `ralphus review pr pull-feedback <pr-id>`, not by hand.
- Don't send anything until every suggestion has a worktree. If one can't be
  placed, ask me about it rather than guessing.
