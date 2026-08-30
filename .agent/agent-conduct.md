# Agent conduct

Rules for any agent (Claude Code, a ralphus cell, a Guardian resolver) doing
work in this repo. Each one is here because ignoring it has already cost a
real session. The root `AGENTS.md` carries the short form; this file carries
the reasoning and the alternative to reach for instead.

## Never start, stop, or restart the daemon or librarian

Do not run `scripts/build-debug.sh`, `ralphus-daemon serve`,
`ralphus-librarian serve`, or anything else that starts or restarts those two
processes unless the user asks for it in that turn. A persistent dev stack is
normally already running.

A failed `ralphus check health` or a connection-refused `curl` on
`127.0.0.1:7890` is **not** sufficient evidence the daemon is down — transient
port/timing hiccups happen. Launching `build-debug.sh` "to help" once picked
up an in-flight squad mid-scheduling and produced duplicate racing cell
starts.

If a `ralphus` command reports a connection error, ask whether the daemon is
expected to be up rather than starting it. If a real *daemon-authored* error
comes back (a validation message, a 4xx body), that is proof the daemon is
reachable — don't let an earlier flaky probe override it. Plain git operations
on worktrees are not "touching the daemon" and are fine.

## Never use `git stash` in this repo

Not bare, not chained with an immediate `pop`, not "just this once", not even
when `git status` shows a clean tree.

The stash stack lives in the shared `.git`, not per-worktree. Many ralphus
task worktrees hang off the same `.git`, and concurrent agents push their own
entries onto that stack. A bare `pop` takes whatever is on top *at that
moment* — which has, in practice, dumped an unrelated task's diff and conflict
markers across files in the wrong worktree. A clean tree makes it worse, not
safer: the push half stores nothing, so the pop half is guaranteed to grab
someone else's entry.

Reach for one of these instead:

- `git diff --stat` / reading the diff — usually already answers "did my
  change touch this area", with zero risk.
- `git show <ref>:path/to/file.rs` or `git diff <ref1> <ref2> -- path` to see
  a file as it stands at another commit.
- `git worktree add <short-scratch-path> <ref>` for a genuinely separate
  checkout when unmodified code must actually be executed (see the MAX_PATH
  gotcha in [`gotchas.md`](gotchas.md) for why the path must be short).

Treat typing `git stash` as a stop-and-ask trigger.

## Comments and docstrings describe the code as it is now

No "used to", no "originally", no "ported from Python", no narrating a `TODO`
that was removed, no explaining a callee's internals. Test: would the sentence
still make sense to someone who never saw a previous version? If not, cut the
framing and state the current structure and reason — "shared by X and Y so
both call the same logic", not "X used to duplicate this".

This applies to code comments and docstrings project-wide. Architecture
narrative in `AGENTS.md` files is a separate case — those documents are meant
to record where things came from.

## Commit messages

Omit the `Claude-Session: https://claude.ai/code/session_...` trailer. Keep
`Co-Authored-By:`.

## Clarifying questions use concrete commands, not terms of art

When a design correction has more than one plausible implementation, asking is
right — but frame the options as literal command sequences or observable
behavior ("the worktree keeps up with pushes; local commits are never lost"),
not ref-mechanics vocabulary like "detached HEAD". Prefer `AskUserQuestion`
options with command-sequence previews over prose descriptions.

## Report what you could not verify

If part of the suite could not run (see the dev-daemon exe lock in
[`gotchas.md`](gotchas.md)), say which tests were skipped and why. Never let
a partial run be reported as full coverage.

Related: [`gotchas.md`](gotchas.md) for the environmental pitfalls these rules
keep bumping into.
