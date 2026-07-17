---
name: submit-tasks
description: Create a series of Ralphus Tasks based on a list of Jira tickets and git repositories
allowed-tools:
  - Bash(ccl *)
  - Bash(get_ticket.ps1 *)
  - Bash(pwsh -Command "ccl *)
  - Bash(pwsh -Command "get_ticket.ps1 *)
  - Bash(pwsh:)
  - PowerShell(ccl *)
  - PowerShell(get_ticket.ps1 *)
  - PowerShell(pwsh -Command "ccl *)
  - PowerShell(pwsh -Command "get_ticket.ps1 *)
---

## Get Jira Ticket Details
We are about to send a list of tasks to `ralphus`.
I will provide all prompts to convert to ralphus Tasks.

I may also define which git repositories affect which tickets.
For example:
```
RGP-1
RGP-2 ~/Documents/some_monorepository ~/repositories/some_special_repository
PIPE-1234 ~/Documents/some_project ~/Documents/some_monorepository
DEV-1412

Use ~/Documents/some_project for everything else.

For ~/Documents/some_monorepository, build the maya project
```

If you get a list of just tickets/tasks to do and no repository data like
above, ASK ME where to look before continuing. If I do not specify how to how
to build the projects, DO NOT continue until I do.


## Build The Repositories Worktrees
Call `/build-worktrees` for each ticket and collect its results.

You should have received some text in the form of
`<git branch name>,<git repository>[,<more git repositories>]`.

Example:
```
RGP-1-add_payment_system,~/Documents/some_project
RGP-2-add_visual_mode,~/Documents/a_different_repository,~/repositories/some_special_repository
PIPE-1234-make_finite_analysis_machine,~/Documents/some_project,~/Documents/a_different_repository
DEV-1412-first_pass,~/Documents/some_project
```


## Build And Submit Tasks
Each of the git branches from before represent a Task-group submission.
It's time to generate TOML file definitions for these tasks.

First we must know how to write the TOML files. Run `ralphus task show-tutor`.
That command explains how to structure the TOML files, and its RECOMMENDED
LAYOUT section is the shape to follow (verbatim ticket prompt -> impl ->
agent-verify -> finalize, one linked review).

Default to 1 TOML file per git branch, each directory an individual task. To
fill out the build, auto-format, lint, and test steps, look at the code in each
directory to figure out what build/lint/test tools exist.

Reorder the Tasks by which should come before/after another, and look for tasks
that can run in parallel. Then ask me if that order looks right.

Before writing, briefly surface the layout trade-offs and let me choose (don't
silently pick). Keep it short:
- **Reviews:** if these branches belong in ONE review, define one `[[review]]`
  and give each `[[task.session]]` the same `review = "ralphus:new-review/<key>"`
  so they link into a single Guardian -- even across separate files. Otherwise
  say plainly "this will create N separate reviews" so I can confirm that's intended.
- **Verifiers:** ask whether the format/lint/test steps should be plain
  `command` gates (pass/fail only) or `prompt` verifiers that AUTO-FIX ("run the
  command; fix and re-run up to 3x; else fail; do not commit/push"). If I asked
  for verification but the draft has no auto-fix, point that out and offer it.
- **Prompt fidelity:** the session `prompt` should be the ticket text pasted
  in as-is (light tweaks only), not your paraphrase.
- **Finalize:** if commits/pushes are wanted, include a finalize AI session that
  stages only source files and commits after the verifiers pass.

Now write the TOML file(s) based on all of the information above.
Then SHOW ME THE TOML text, in full! Stop to ask me if I want to make any final changes.

Next, call `ralphus validate /path/to/file.toml /another/file/to.toml /and/more/file.toml`
and fix any errors or warnings. Repeat this until the .toml file runs with no issues.

Lastly, call `ralphus submit /path/to/file.toml` and confirm if it submitted
successfully.
