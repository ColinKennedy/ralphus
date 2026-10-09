# Embedding tmux (RAL-102, vendorized in RAL-347)

## Headless by default

Cells, proof steps and the review's resolver/feedback/summary runs are
**headless**: `ralphus-runner send` drives the agent over piped stdio, so
nothing needs a terminal. `daemon/src/runner/direct.rs` spawns it as a plain
child process, tees its stdout and stderr into the cell's capped `.raw`
transcript (the file a `pipe-pane` sink used to write), and takes completion
from the child's exit status and its result file. Event forwarding, stall
detection, terminal logs and the board's Live View all read that transcript,
so a running cell's Live View is the live tail of its transcript, not a
rendered screen. Cancelling, detaching, timing out and a cost cap kill the
child's whole process tree (`crate::proof::ProcessTree`: a Job Object on
Windows, a process group on Unix).

`RALPHUS_RUNNER_MODE=tmux` restores the older path, where each run lives in a
detached tmux session; `direct` forces the headless path for every spec. The
rest of this document describes that tmux machinery, which is still used by
**Open Agent**: detaching a running cell stops its headless run, then
`<cell>-resume` is a tmux session running the agent's own `--resume
<agent_session_id>`, so a human can take over and "Resume Automation" hands the
same conversation back to a headless run.

## The tmux path

When a run does go through tmux (`RALPHUS_RUNNER_MODE=tmux`, or an Open Agent
resume session), the agent runs in a detached tmux session so the board can
show a live, pollable view of what it is doing. See `daemon/src/tmux.rs` for
the wrapper and `daemon/src/runner.rs`'s `SubprocessRunner::run_via_tmux` for
how a session is launched and its result collected back.

On Windows this is [psmux](https://github.com/psmux/psmux), a native
Windows tmux alternative — not real tmux, and not a drop-in (see "Why
`respawn-pane` is worked around" below).

## Vendored by default (RAL-347)

psmux's **source** is vendored into this repo as a git submodule at
`vendor/psmux`, pinned to a specific upstream commit (currently `v3.3.8` —
see `.gitmodules`). No compiled binary is checked in; only the pinned source
tree is. This closes a supply-chain gap: previously the only way to get a
bundled tmux binary was to trust whatever the build machine had installed
system-wide (WinGet, PATH, etc.), which is a plausible place for a
compromised or tampered binary to intercept everything an agent session
does, including model API keys. Building from a pinned, reviewable source
tree in this repo removes that trust dependency.

Because it's the *source* that's vendored, not a redistributed binary,
licensing isn't a redistribution concern the way it would be for a checked-in
`tmux.exe` — the license that applies is whatever the pinned submodule commit
carries (currently MIT), and it can change across future version bumps or
downgrades of the submodule without this repo needing to re-clear anything.

## Resolution order

`daemon/src/tmux.rs::resolve_tmux_program` picks a tmux-compatible binary in
this order:

1. `RALPHUS_TMUX_CMD` env var, if set — an explicit, deliberate override,
   skipping everything below. This is the supported way to point ralphus at
   your own tmux/psmux binary instead of the vendored one.
2. The embedded build vendored from `vendor/psmux`, if this build was
   compiled with `--features ralphus-daemon/embedded-tmux` and
   `daemon/assets/tmux/windows/tmux.exe` was populated by
   `scripts/build-vendored-tmux.ps1` beforehand.
3. `tmux` resolved on `PATH`, as a last resort.

If none of these resolve, the daemon returns a clear error rather than
panicking or silently no-opping.

**Note the order change from RAL-102**: the embedded/vendored build is now
checked *before* `PATH`, not after. A build with `embedded-tmux` enabled
trusts its own pinned, in-repo build over whatever a machine happens to have
installed under the name `tmux` — that priority is itself part of the
supply-chain hardening this ticket is about. If you need something else,
`RALPHUS_TMUX_CMD` is the explicit way to say so; see
[`docs/dependencies.md`](dependencies.md) for why relying on a separately
installed, non-vendored tmux/psmux is discouraged.

## Building and enabling the vendored binary

1. Run `scripts/build-vendored-tmux.ps1` (Windows only). It initializes the
   `vendor/psmux` submodule if needed, runs `cargo build --release
   --manifest-path vendor/psmux/Cargo.toml --bin tmux` (psmux is itself a
   Rust project), and copies the resulting `tmux.exe` to
   `daemon/assets/tmux/windows/tmux.exe`.
2. Rebuild with `cargo build --release --features ralphus-daemon/embedded-tmux`.
   The Windows paths in `scripts/build-release.cmd` and `build-release.sh` do
   both steps by default — pass `--skip-tmux` to opt out and build without it (e.g. no
   network access to fetch the submodule's own crate dependencies, or you
   always point `RALPHUS_TMUX_CMD` at your own binary anyway). Vendorization
   is optional, never mandatory: a plain `cargo build`/`cargo test` with no
   extra flags never needs the submodule at all, since `embedded-tmux` is off
   by default.
3. Verify: unset `RALPHUS_TMUX_CMD`, temporarily remove `tmux` from `PATH`,
   and confirm the daemon still starts task cells successfully via the
   embedded/vendored fallback.

Automated GitHub releases (`.github/workflows/release.yml`) always build with
the vendored psmux included, via `scripts/build-bundle.py` →
`scripts/build-release.cmd`'s default behavior — so a downloaded release
bundle works out of the box with no separate tmux/psmux install.

## Why `respawn-pane` is worked around

Windows tmux alternatives have real behavioral gaps versus upstream tmux —
empirically confirmed while implementing RAL-102: `respawn-pane <command>`
exits successfully on the Windows psmux build this project targets but does
**not** actually replace the pane's shell with the given command (the shell
prompt is left running); the code works around this by falling back to
`send-keys <command> Enter` on Windows only (see
`Tmux::new_detached_session_with_command`). This is a build-specific gap
unrelated to which copy of psmux is running (vendored or external) — it's
about the psmux project's own compatibility with upstream tmux, not about
where the binary came from.

macOS and Linux use real, unmodified tmux (see `docker/Dockerfile` and
`docs/container-mode.md`) — vendorizing/embedding is a Windows-only concern
because psmux is Windows-only; macOS/Linux embedding is planned but not
required for this ticket's Windows-first scope, and would follow the same
`daemon/assets/tmux/<platform>/` + `#[cfg(target_os = "...")]` pattern once a
source to vendor there is chosen.
