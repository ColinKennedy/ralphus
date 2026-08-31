# Simple task templates (RAL-297)

The board's `+ New Task` modal has a **Simple** tab: a deterministic,
zero-LLM-overhead form that submits a single task built from a `work` +
`finalize` cell pair, mirroring the pattern `ralphus task show-tutor`
documents. Simple submissions are template-driven, and templates are
declared in `.ralphus.toml` under `[[templates]]`.

Every Simple submission always collects five fixed base fields --
`prompt`, `agent`, `model`, `project`, and `proofs` -- regardless of which
template is selected. A `[[templates]]` entry only adds *supplementary*
fields and a `prompt_template` that combines them (and the base `prompt`)
into the text the work cell actually runs.

## Schema

```toml
[[templates]]
name        = "hello-world"    # required, unique, the picker's <option> value
label       = "Hello World"    # optional, falls back to `name`
description = "Minimal one-shot task: run a prompt as-is, no extra context."
prompt_template = "{prompt}"   # required; {prompt} plus any declared field name

[[templates]]
name        = "standard"
label       = "Standard Task"
description = "A prompt plus optional ticket/context references."

[[templates.fields]]
name     = "ticket_id"
label    = "Ticket ID"
type     = "string"   # one of "string" / "number" / "bool"
required = false

[[templates.fields]]
name     = "context_files"
label    = "Context files (comma-separated paths)"
type     = "string"
required = false

prompt_template = """
{prompt}

Ticket: {ticket_id}
Relevant files: {context_files}
"""
```

A field's `type` drives client-side validation in the form (a `number`
field rejects non-numeric input, a `required` field blocks submission when
blank); `prompt_template` placeholders are validated at config-load time
against the field's own declared name plus the fixed `{prompt}` -- an
unknown placeholder, an empty `prompt_template`, or a duplicate
template/field name is flagged by `ralphus check health` rather than
silently breaking the picker.

## No templates configured

If a project (and the global config) together declare zero `[[templates]]`
entries, the Simple tab falls back to a single built-in template identical
to the `hello-world` example above. In that state the template-selection
dropdown is disabled and shows a tooltip explaining that no templates are
configured, so a user can't confuse the fallback for an authored choice.

A malformed individual `[[templates]]` entry (missing `prompt_template`, a
duplicate name, an unknown field type or placeholder) is dropped from the
picker rather than blocking every other template -- `ralphus check health`
still reports it as a failure so it doesn't go unnoticed.

## Default tab

By default the `+ New Task` modal opens to the Simple tab. A project can
change that with:

```toml
[ui]
new_task_default_tab = "paste"   # "simple" (default) / "files" / "paste"
```

## What gets submitted

The Simple tab never hand-authors TOML by itself in the LLM sense --
substitution is pure string formatting, and the generated task/cell
structure is fixed: a `work` cell (the substituted prompt, the chosen
agent/model, an optional `proofs` list) that runs in a fresh
`ralphus:new-worktree/<branch>?upstream=<upstream>` worktree for the chosen
project, plus a `finalize` cell that stages/commits/pushes, matching
`ralphus task show-tutor`'s recommended per-branch layout and reusing its
system prompts verbatim. The assembled TOML is submitted through the same
`POST /api/squads` endpoint the Paste tab uses -- so the only difference
between Simple and Paste is *how* the TOML text is produced, not how it's
validated or run.

The only LLM calls the Simple tab ever makes are the explicit, opt-in
"Generate Proofs" / "Generate Manual Checks" buttons (`POST /api/generate`)
-- everything else, including template substitution and TOML assembly, is
deterministic string formatting with no model call involved.
