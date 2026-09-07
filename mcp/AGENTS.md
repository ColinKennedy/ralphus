# mcp/

`ralphus-mcp`: an MCP server exposing ralphus's daemon HTTP API as MCP tools
(RAL-301), so an MCP client (Claude Code, other agent hosts) can drive
squads/tasks/cells/proofs/reviews/etc. without the `ralphus` CLI binary
installed at all. Talks to the daemon directly through `ralphus-cli`'s
`DaemonClient`, reused as a library (`cli` is both a `[[bin]]` and a
`[lib]`) -- this crate never shells out to the compiled `ralphus` binary.

## Module map

- `src/protocol.rs` -- the MCP stdio transport: newline-delimited JSON-RPC
  2.0 over stdin/stdout (`initialize`/`tools/list`/`tools/call`/`ping`).
  Hand-rolled, matching this workspace's existing house style of
  hand-rolling thin protocol layers (`daemon/src/server.rs`'s HTTP routing)
  rather than adopting a framework dependency. `write_message` is the *only*
  place in this crate allowed to write to stdout (mirrors `runner/`'s own
  stdout-is-a-wire-contract discipline, and this workspace's
  `clippy::print_stdout = "deny"` lint) -- stdout here carries the MCP
  JSON-RPC replies, not log output.
- `src/tools.rs` -- builds the MCP tool registry directly from
  `ralphus_cli::help_map::registered_leaves()` (name, description,
  JSON-Schema `inputSchema`, `read_only` flag), so the tool surface can never
  drift from `help_map.rs`'s tree by hand -- see `src/chip.rs` for how a
  chip string (`"selector [str]"`, `"--from [index]"`, `"forge
  [github|gitlab]"`, ...) becomes a JSON-Schema property. `Tool::build_argv`
  converts an MCP tool call's JSON `arguments` back into the argv
  `ralphus_cli::commands::parse_args` expects, so argument
  parsing/validation is still owned entirely by `cli`, never
  re-implemented here.
- `src/exclusions.rs` -- the documented-reason exclusion list for CLI leaves
  that are genuinely not portable to one MCP request/response call (spawn an
  interactive, TTY-attached agent session: `cell open-agent`, every
  `quick-start` backend). **Whenever a new CLI leaf is added that can't be
  wired to a real MCP tool, add it here with a real reason** -- an empty or
  placeholder reason fails `mcp/tests/parity.rs`'s
  `every_exclusion_has_a_non_empty_substantive_reason` test. This is not an
  escape hatch for "not implemented yet" -- see that module's doc comment.
- `src/exec/` -- one file per CLI command group (`task.rs`, `cell.rs`,
  `review.rs`, ...), each mirroring that group's `ralphus_cli::commands::*`
  `dispatch` match arms one-for-one, but building a `serde_json::Value`
  response instead of printing (`dispatch` prints because CLI stdout *is*
  its product; an MCP tool call returns a value instead). Argv
  parsing/selector resolution is fully reused from `cli`
  (`commands::parse_args`, `selector::resolve_*`) -- only the final "call
  `DaemonClient`, shape a `Value`" step is duplicated, since that's the one
  place `dispatch` is inherently print-shaped. See `exec/mod.rs`'s doc
  comment for the fuller rationale, including why a generic
  print-capture/stdout-redirect bridge was considered and rejected (this
  workspace forbids `unsafe_code` workspace-wide, and there's no safe
  cross-platform way to redirect a process's own stdout on Windows without
  it; a stray `println!` reused from `cli` would also corrupt this
  crate's own stdout-is-the-MCP-wire contract).
- `src/server.rs` -- wires `tools.rs` + `exec/` into the three MCP methods;
  owns the `--read-only` filter (only `read_only`-tagged tools are
  listed/callable) mirroring `cli`'s own `quick-start ... --read-only`
  mechanism (see `cli/AGENTS.md`'s Read-Only Quick-Start Safety List).

## Adding a new CLI command

Since `src/tools.rs` derives the tool list mechanically from
`help_map::registered_leaves()`, a new leaf in `cli/src/help_map.rs`
automatically gets an MCP tool with a schema for free -- the one thing that
does **not** happen for free is execution: add a matching arm to the right
`src/exec/*.rs` file (or `exclusions.rs`, with a real reason, if it's
genuinely not portable). `mcp/tests/parity.rs` fails locally and in CI the
moment a leaf has neither.

## Parity check (RAL-301's core requirement)

`mcp/tests/parity.rs` is a normal `#[test]`, covered by the same
`cargo nextest run --all-targets` root `AGENTS.md` already asks developers to run
and `.github/workflows/ci.yml`'s `rust` job already runs -- there is no
separate CI-only parity script. It checks, bidirectionally, against
`help_map::registered_leaves()` (the same tree `cli`'s own
`help_map_command_parity.rs` proved is real and dispatchable):

- every non-excluded CLI leaf has a corresponding tool
- every tool corresponds to a real leaf, and every tool's synthesized argv
  round-trips through `commands::parse_args` into a real command (not a
  `UsageError`/`Help` fallback)
- every exclusion names a real leaf and carries a substantive reason
