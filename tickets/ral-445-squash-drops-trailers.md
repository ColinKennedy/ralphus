# RAL-445 loses trailers on squash

**Status:** open
**Found by:** prophecy subsystem design (`docs/prophecy-design.md`, §8.2 / §13), 2026-09-25
**Component:** daemon

## Problem

`squash_review_commits` (`daemon/src/guardian_merge.rs:10335`) commits the
squashed result with `--no-verify` (line 10361), which skips the
`prepare-commit-msg` hook that RAL-445 installs to append `Co-authored-by:`
(and any future `Ralphus-Cell:`) trailers. It also rebuilds the squashed
commit message from `git log --reverse --format=%s` (line 10350) — subjects
only — so even trailers that *did* make it onto the individual squashed
commits are dropped from the message being rebuilt.

Net effect: squashing a review branch discards every trailer on every
squashed commit, regardless of whether the hook ran on the way in.

## Suggested fix

Either:
- Collect and re-append the union of trailers from the squashed commits'
  original messages (e.g. via `git log --format=%(trailers)`) when building
  the new squashed message, instead of subjects only; or
- Run the squash commit through the normal hook path (drop `--no-verify`)
  once the message includes the right sources to re-derive the trailers from.

## Not being fixed here

This ticket records the gap; it is not fixing it. See
`docs/prophecy-design.md` §13 for the context that surfaced it.
