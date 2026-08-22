# Embedding tmux (RAL-102)

ralphus routes every agent invocation (task cells, `prompt`-kind proof
steps, Guardian merge/resolver cells) through a detached tmux session
instead of a raw child process, so the board can show a live, pollable view
of what the agent is doing. See `daemon/src/tmux.rs` for the wrapper and
`daemon/src/runner.rs`'s `SubprocessRunner::run_via_tmux` for how a session
is launched and its result collected back.

## Resolution order

`daemon/src/tmux.rs::resolve_tmux_program` picks a tmux-compatible binary in
this order:

1. `RALPHUS_TMUX_CMD` env var, if set — an explicit override, skipping
   everything below.
2. `tmux` resolved on `PATH`.
3. An embedded fallback binary, if this build was compiled with
   `--features ralphus-daemon/embedded-tmux` and a binary has been placed at
   `daemon/assets/tmux/<platform>/`.

If none of these resolve, the daemon returns a clear error rather than
panicking or silently no-opping.

## Why embedding isn't done yet

The ticket (RAL-102) explicitly calls out two things that must be verified
before bundling a binary, not assumed:

- **It's not a drop-in of upstream tmux.** ralphus targets Windows first.
  Windows tmux alternatives (e.g. the "psmux" port referenced in the gastown
  reference repo's `internal/tmux/tmux.go`) have real behavioral gaps versus
  upstream tmux — empirically confirmed while implementing this ticket:
  `respawn-pane <command>` exits successfully on the Windows build tested
  here but does **not** actually replace the pane's shell with the given
  command (the shell prompt is left running); the code works around this by
  falling back to `send-keys <command> Enter` on Windows only (see
  `Tmux::new_detached_session_with_command`). Any *different* Windows tmux
  build might have different gaps, so a chosen binary must be tested against
  the exact code paths in `daemon/src/tmux.rs` before being trusted.
- **License and linkage need confirming.** Redistributing a compiled
  `tmux.exe` inside `dist/` requires knowing its license permits
  redistribution, and whether it's statically or dynamically linked (a
  dynamically linked binary needs its DLL dependencies embedded and
  extracted too, or it silently fails on machines that lack them).

Both are one-time, deliberate decisions for a human to make — not something
to embed blind. `daemon/assets/tmux/windows/README.md` tracks the checklist;
`daemon/src/tmux.rs`'s `embedded-tmux` feature is off by default until that
checklist is done, and the `embedded::extract()` fallback compiles to a
clear runtime error (not a build failure) in every configuration that lacks
the asset.

## Enabling embedding once a binary is sourced

1. Complete the checklist in `daemon/assets/tmux/windows/README.md` and place
   the confirmed `tmux.exe` there.
2. Rebuild with `cargo build --release --features ralphus-daemon/embedded-tmux`
   (or add the feature to `scripts/build-release.cmd`'s daemon build step).
3. Verify: unset `RALPHUS_TMUX_CMD`, temporarily remove `tmux` from `PATH`,
   and confirm the daemon still starts task cells successfully via the
   embedded fallback.

macOS and Linux embedding follow the same pattern
(`daemon/assets/tmux/<platform>/`, a matching `#[cfg(target_os = "...")]` arm
in `embedded::extract()`) once a build for those platforms is sourced and
verified the same way; they are planned but not required for this ticket's
Windows-first scope.
