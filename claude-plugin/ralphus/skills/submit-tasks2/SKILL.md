---
name: submit-tasks2
description: Create a series of Ralphus Tasks based on a list of Jira tickets and git repositories, using registered projects and placeholder worktree cwds instead of manually built worktrees
allowed-tools:
  - Bash(ccl *)
  - Bash(get_ticket.ps1 *)
  - Bash(pwsh -Command "ccl *)
  - Bash(pwsh -Command "get_ticket.ps1 *)
  - Bash(pwsh:)
  - Bash(ralphus *)
  - PowerShell(ccl *)
  - PowerShell(get_ticket.ps1 *)
  - PowerShell(pwsh -Command "ccl *)
  - PowerShell(pwsh -Command "get_ticket.ps1 *)
  - PowerShell(ralphus *)
---

## Get Task Details
I will provide all prompts to convert to ralphus Tasks.

Prompts usually come in one of three forms

1. Semantic, Assuming - "Add a payment system to some_project"
2. Explicit
```
RGP-1
RGP-2 ~/Documents/some_monorepository ~/repositories/some_special_repository
PIPE-1234 ~/Documents/some_project ~/Documents/some_monorepository
DEV-1412

Use ~/Documents/some_project for everything else.

For ~/Documents/some_monorepository, build the maya project
```
3. A combination of (1) and (2).

When you receive a prompt like (1), it's your job to figure out the project path.

Use `ralphus project list --short` to view project details. If you see a close
match to what the user wrote, use `ralphus project get <name>` to get the path
on-disk where its code lives.

If you cannot auto-find the project details for any task, ASK ME where to look
before continuing.


## Build And Submit Tasks

Each ticket's branch represents a Task-group submission. It's time to
generate TOML file definitions for these tasks.

First we must know how to write the TOML files. Run `ralphus task show-tutor`.
Use the recommended task layout in the tutor.

Default to 1 TOML file per git branch, each worktree is an individual task.

To fill out the build, auto-format, lint, and test session-verify steps, look
at the code in each repository to figure out what build/lint/test tools exist.

Reorder the Tasks by which should come before/after another, and look for tasks
that can run in parallel. Then ask me if that order looks right.

Before writing, briefly surface the layout trade-offs and let me choose (don't
silently pick). Keep it short:
- **Agents:** Ask me what agent + model should be used
  (e.g. `"claude-code" / "sonnet"`). Use `ralphus agents list`
  to get the full list of supported agent+model combinations.
- **Reviews:** if these branches belong in ONE review or several.
- **Verifiers:** ask whether the format/lint/test steps should be plain
  `command` gates (pass/fail only) or `prompt` verifiers that AUTO-FIX ("run
  the command; fix and re-run up to 3x; else fail; do not commit/push").
  Recommend `prompt` verifiers. If I asked for verification but the draft has
  no auto-fix, point that out and offer it.
- **Prompt fidelity:** the session `prompt` should be the ticket text pasted
  in as-is (light tweaks only), not your paraphrase.
- **Finalize:** if commits/pushes are wanted, include a finalize AI session that
  stages only source files and commits after the verifiers pass.

Now write the TOML file(s) based on all of the information above.
Then SHOW ME THE TOML text, in full! Stop to ask me if I want to make any final changes.

Next, call `ralphus validate /path/to/file.toml /another/file/to.toml /and/more/file.toml`
and fix any errors or warnings.

Lastly, call `ralphus submit /path/to/file.toml /another/file/to.toml /and/more/file.toml`
and confirm if it submitted successfully. The daemon builds each session's
worktree the first time it runs -- you do not need to (and should not)
pre-create it.
