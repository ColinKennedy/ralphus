---
name: submit-tasks
description: Create a series of Claudectl Tasks based on a list of Jira tickets and git repositories
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
  - mcp__plugin_claudectl_claudectl-bus__preview_guardian_batch
  - mcp__plugin_claudectl_claudectl-bus__stage_guardian_batch
---

## Get Jira Ticket Details
Send a list of tasks to `ralphus`. I will provide all prompts to convert to Tasks.

I may also define which git repositories affect which tickets.
For example:
```
RGP-1
RGP-2 ~/Documents/some_monorepository ~/repositories/some_special_repository
PIPE-1234 ~/Documents/claudectl ~/Documents/some_monorepository
DEV-1412

Use ~/Documents/claudectl for everything else.

For ~/Documents/some_monorepository, build the maya project
```

If you get a list of just Jira ticket IDs and no repository data, ask me where
to look before continuing. If I do not specify how to how to build the
projects, DO NOT continue until I do.

## Build The Repositories Worktrees
Call `/build-worktrees` for each ticket and collect its results.

You should have received some text in the form of
`<git branch name>,<git repository>[,<more git repositories>]`.

Example:
```
RGP-1-add_payment_system,~/Documents/claudectl
RGP-2-add_visual_mode,~/Documents/a_different_repository,~/repositories/some_special_repository
PIPE-1234-make_finite_analysis_machine,~/Documents/claudectl,~/Documents/a_different_repository
DEV-1412-first_pass,~/Documents/claudectl
```


## Build And Submit Tasks
Each of the git branches from before represent a Task-group submission.
It's time to generate TOML file definitions for these tasks.

First we must know the general syntax for the TOML files.
Run `ralphus task show-tutor`. Its printout will explain.

I want you to make 1 TOML file per git branch and each directory can be an
individual task. To fill out the build, test, and steps, look at the code in
each directory to figure out the common build, lint, etc tools exist.

Summarize the TOML text and then SHOW ME THE TOML text, in full! Stop to ask me
if I want to make any changes.

Ask me if I want to add verification steps for auto formatting, linting, and
testing. I will add these as individual AI prompt steps.

Using the TOML example above, now call
`mcp__plugin_claudectl_claudectl-bus__validate_tomls` to check if there are any
issues with the TOML. That command may come back with warnings or errors. Try
again until validation passes.

Once you have a list of validated TOML files to submit, call
`mcp__plugin_claudectl_claudectl-bus__submit_tomls`.
