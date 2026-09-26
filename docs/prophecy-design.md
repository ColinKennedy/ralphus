## 13. Tickets to file regardless of this subsystem

Both are pre-existing RAL-445 bugs surfaced while designing §8.2. Phase 4's
trailer work is blocked on them, but they are worth fixing either way.

1. **RAL-445 does not reach remote worktrees.** `sync_coauthor_hook` is only
   called from the local `git worktree add` path, so every commit made on a
   remote machine is silently missing its `Co-authored-by:` attribution. Fix:
   port `git_hooks.rs` from `&Path` to `&Workspace` (§6.3).
2. **RAL-445 loses trailers on squash.** `squash_review_commits`
   (`daemon/src/guardian_merge.rs:10402`) commits with `--no-verify` — skipping
   the hook — and rebuilds its message from `git log --format=%s`, subjects
   only. Squashing a review branch therefore discards every trailer on every
   squashed commit.

Optional third, smaller: `ghost::current_revision` is local-only (§6.3).
