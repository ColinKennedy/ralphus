---
name: build-worktrees
description: "DEPRECATED (RAL-100): manually creates a git worktree per repository and reports back a real path for use as task cwd. Prefer /submit-tasks2, which registers the repository as a project and lets the daemon materialize the worktree from a placeholder cwd instead. Still used by /submit-tasks for callers who haven't migrated."
allowed-tools:
  - Bash(git *)
  - Bash(mkdir *)
  - Bash(test *)
  - PowerShell(git *)
  - PowerShell(New-Item *)
  - PowerShell(Test-Path *)
---

> **DEPRECATED (RAL-100).** This skill manually builds a worktree and hands
> back a literal filesystem path -- a recurring, error-prone manual step.
> Prefer `/submit-tasks2`, which registers the repository once with
> `ralphus project git` and emits a placeholder session `cwd`
> (`"ralphus:new-worktree/<branch>"`, with the task's `project` field naming
> the registered repository); the daemon then materializes (or reuses) the
> worktree deterministically before the session runs. This
> skill is kept only because `/submit-tasks` still calls it -- new work
> should use `/submit-tasks2` instead.

## Inputs

This skill handles **one ticket** per call. It needs three things:

1. A ticket id (e.g. `RGP-1`, `PIPE-1234`).
2. A short description of the work, to build the branch slug from (e.g. "add payment system").
3. One or more git repositories that the ticket touches, which will each get its own worktree.

Get these from, in order of preference:
- The `args` passed to this skill (free text — parse whatever ticket id / description / repo paths appear in it).
- The calling conversation's context, if this was invoked per-ticket from another skill (e.g. `submit-tasks`) that already gathered ticket/repo info.
- If neither is available, ASK ME for the ticket id, a one-line description, and the repositor(y/ies) before continuing. Do not guess a repository.

## Compute the branch name

Build the branch name as `<TICKET-ID>-<slug>` e.g. `PIPE-1234-some_description`:
- Lowercase the description.
- Replace runs of whitespace/punctuation with a single underscore.
- Trim leading/trailing underscores.
- Keep it reasonably short (aim for under ~40 characters for the slug portion; drop trailing words rather than truncating mid-word).

Example: ticket `RGP-1`, description "Add payment system" → `RGP-1-add_payment_system`.

## Build a worktree per repository

For each repository given:

1. Expand `~` and resolve to an absolute path. Confirm it exists and is a git repo (`git -C <repo> rev-parse --is-inside-work-tree`). If it isn't, stop and tell me which repo failed — do not skip it silently.
2. Pick the worktree path: `<repo>/.worktrees/<branch>` (a hidden, per-repo, per-branch directory — safe to `.gitignore`, doesn't collide with other tickets' worktrees in the same repo).
3. Check `git -C <repo> worktree list` first:
   - If a worktree already exists at that path, reuse it (do nothing further for this repo).
   - Otherwise, check whether `<branch>` already exists (`git -C <repo> branch --list <branch>`):
     - Branch exists → `git -C <repo> worktree add "<worktree-path>" <branch>`
     - Branch doesn't exist → `git -C <repo> worktree add -b <branch> "<worktree-path>"` (branches off the repo's current `HEAD` unless I told you otherwise).
4. If `git worktree add` fails (dirty state, branch checked out elsewhere, etc.), stop and show me the error — don't paper over it or fall back to something else.

## Report the result

Once every repository for this ticket has a worktree, output **exactly one line**, and nothing else around it (no extra commentary — the caller parses this line):

```
<branch>,<worktree-path-1>[,<worktree-path-2>...]
```

Use the worktree paths from step 2 above (not the original repo paths) — those are the real, isolated working directories the caller should use as task `cwd`s for this branch.

Example, for ticket `RGP-1` / repo `~/Documents/some_project`:

```
RGP-1-add_payment_system,~/Documents/some_project/.worktrees/RGP-1-add_payment_system
```

If called once per ticket across several tickets, each call still emits just its own single line — the caller is responsible for collecting one line per ticket, e.g.:

```
RGP-1-add_payment_system,~/Documents/some_project/.worktrees/RGP-1-add_payment_system
RGP-2-add_visual_mode,~/Documents/a_different_repository/.worktrees/RGP-2-add_visual_mode,~/repositories/some_special_repository/.worktrees/RGP-2-add_visual_mode
PIPE-1234-make_finite_analysis_machine,~/Documents/some_project/.worktrees/PIPE-1234-make_finite_analysis_machine,~/Documents/a_different_repository/.worktrees/PIPE-1234-make_finite_analysis_machine
DEV-1412-first_pass,~/Documents/some_project/.worktrees/DEV-1412-first_pass
```
