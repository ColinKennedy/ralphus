# cli-rs/

The `ralphus` CLI: a thin HTTP client over the daemon's API (`client.rs`, all
77 methods), ~105 leaf subcommands under `commands/`. See the root
`AGENTS.md`'s Architecture table and "The Rust CLI/runner port" section for
the module map and the disclosed `ralphus author` gap.

`ralphus-mcp` (`../mcp/`, RAL-301) depends on this crate as a library and
mirrors `commands/*.rs`'s `dispatch` match arms to build MCP tool responses
(see `mcp/src/exec/*.rs`) -- that's why a number of otherwise-private
per-command helpers here (`resolve_scoped`, `with_uri`, `proof_step_for`,
`agent_resume_command`, and similar) are `pub` despite having exactly one
call site inside this crate's own `dispatch`. Don't re-privatize one of these
without checking `mcp/src/exec/` for a caller first.

## Read-Only Quick-Start Safety List (RAL-194)

Each `HelpNode` in `cli-rs/src/help_map.rs`'s command tree (`ROOT` and its
children) carries a `read_only_safe: bool` field — the allowlist a
`--read-only` quick-start session (manager/reviewer/watcher) is told it may
call. The tag describes a property of the command itself (it performs no
mutation under any of its own flags), not a permission gate — a normal
(non-read-only) session may call any command, tagged or not. Only a
`--read-only` session is restricted, and for that session the injected
help-map tree is pruned down to `(read-only-safe)` commands only (plus the
group headers needed to reach them) via `help_map::generate_read_only_safe`
— see `quick_start.rs`'s `help_map_tree` — rather than merely tagging
everything and trusting the model to self-filter.

**Whenever you add a new `ralphus` CLI subcommand, decide whether it is safe
to run under `--read-only`, and if so, set its `HelpNode`'s `read_only_safe`
to `true`.** A command belongs on the list only if it performs no mutation
under *any* of its own flags — it queries the daemon or local files and
prints, never writes (a command that only prints an action for a human to
run themselves, like `review checks run`, still counts as non-mutating).
Everything else — including any new
`set-status`/`restart`/`edit`/`create`/`delete`/`cancel`/`merge`/`approve`/`feedback`/`register`/`remove`/`git`,
`submit`, or `clear`-shaped command — must be left `false` (unsafe-by-default).
Don't default to leaving a new command off the list out of habit; make the
call explicitly, the same way the Bench Patience rule
([[../bench-harness/AGENTS|bench-harness/AGENTS.md]]) asks you to deliberately
decide on `patience` for every new benchmarked test.

Logging conventions for `cli-rs/src/main.rs`'s `cli` log type are documented
centrally at [[../.agent/logging-policy|logging-policy.md]].
