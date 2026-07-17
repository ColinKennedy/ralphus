# Embedded Windows tmux binary — not yet populated

This directory is where a verified `tmux.exe` must be placed before building
with `--features embedded-tmux` (see `daemon/src/tmux.rs`'s `embedded` module
and `docs/tmux-embedding.md`). It is intentionally empty in this commit.

Before adding a binary here:

1. Confirm which Windows tmux build is being embedded (upstream tmux via
   MSYS2/Cygwin, or a Windows-native alternative such as the "psmux" port
   referenced in `internal/tmux/tmux.go` of the gastown reference repo) —
   do not assume any given `tmux.exe` is a drop-in; the `respawn-pane`
   command-argument gap alone proves behavior can differ from upstream tmux.
2. Confirm its license permits redistribution inside `dist/`, and record the
   license text/attribution alongside this file.
3. Confirm whether it is statically or dynamically linked. A dynamically
   linked binary needs its DLL dependencies embedded and extracted too, or it
   will fail to run on a machine that lacks them.
4. Place the confirmed binary at `daemon/assets/tmux/windows/tmux.exe` and
   rebuild with `--features embedded-tmux`.

Until this is done, `ralphus-daemon` still works fully when `tmux` is
reachable on `PATH` (the default resolution order in
`daemon/src/tmux.rs::resolve_tmux_program`) or via the `RALPHUS_TMUX_CMD`
override; only the "no separate install step" embedding AC is pending on this
file being populated.
