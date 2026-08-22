# cli-rs/

The `ralphus` CLI: a thin HTTP client over the daemon's API (`client.rs`, all
77 methods), ~105 leaf subcommands under `commands/`. See the root
`AGENTS.md`'s Architecture table and "The Rust CLI/runner port" section for
the module map and the disclosed `ralphus author` gap.

## Read-Only Quick-Start Safety List (RAL-194)

Each `HelpNode` in `cli-rs/src/help_map.rs`'s command tree (`ROOT` and its
children) carries a `read_only_safe: bool` field — the allowlist a
`--read-only` quick-start session (manager/reviewer) is told it may call. It
drives the `(read-only-safe)` tag shown in the injected help-map (see
`READ_ONLY_NOTE` in that same file) and the model is instructed to only
invoke tagged commands while mutating ones stay off-limits.

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
