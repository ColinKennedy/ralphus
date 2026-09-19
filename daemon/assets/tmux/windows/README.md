# Embedded Windows tmux binary — built, not checked in

This directory is where `scripts/build-vendored-tmux.ps1` writes a `tmux.exe`
built from the vendored `vendor/psmux` git submodule (RAL-347) before
building with `--features embedded-tmux` (see `daemon/src/tmux.rs`'s
`embedded` module and `docs/tmux-embedding.md`). The binary itself is
gitignored (`.gitignore`'s `/daemon/assets/tmux/windows/tmux.exe` entry) and
is not present in a fresh checkout — only the pinned submodule *source* is
vendored, never a compiled binary.

To populate it:

```powershell
scripts\build-vendored-tmux.ps1
cargo build --release --features ralphus-daemon/embedded-tmux
```

`scripts\build-release.cmd` runs both steps by default (opt out with
`--skip-tmux`), which is how release/GitHub-release bundles
ship the embedded build out of the box.

## Why this used to require a manual checklist (RAL-102), and doesn't anymore

Embedding a `tmux`-compatible binary used to require a human to manually
source, verify, and place a `tmux.exe` here before three things could be
confirmed:

1. **Which Windows tmux build, and is it a behavioral drop-in?** Answered:
   `vendor/psmux`, pinned to a specific upstream commit (currently `v3.3.8`)
   — not assumed compatible; `docs/tmux-embedding.md` documents the one known
   behavioral gap (`respawn-pane`) and how the code already works around it.
2. **Does its license permit redistribution inside `dist/`?** No longer a
   redistribution question at all: this repo only vendors the submodule's
   *source*, never a compiled binary. Whatever license the pinned commit
   carries (currently MIT) applies to that source, and can change across
   future version bumps/downgrades of the submodule without needing
   re-clearing — see `docs/tmux-embedding.md`.
3. **Static or dynamic linkage?** psmux is a Rust project built with `cargo
   build --release`; the resulting `tmux.exe` is a normal Rust release
   binary with no separate DLL dependencies to track down.

Until `scripts/build-vendored-tmux.ps1` has been run, `ralphus-daemon` still
works fully via `RALPHUS_TMUX_CMD` or a `tmux`/psmux binary reachable on
`PATH` (see the resolution order in `daemon/src/tmux.rs::resolve_tmux_program`
and `docs/tmux-embedding.md`) — only the "no separate install step" embedding
AC needs this file populated, and vendorization is optional, not mandatory.
