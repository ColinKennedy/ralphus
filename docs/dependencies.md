# External runtime dependencies

Tools ralphus expects to find on `PATH` (or via an explicit override env
var) at runtime, and any version constraints that matter.

## tmux / psmux (Windows)

**Windows requires psmux 3.3.6 or later.** `daemon/src/tmux.rs` resolves a
`tmux`-named binary on `PATH` (or `RALPHUS_TMUX_CMD` if set — see
`docs/tmux-embedding.md`) for every agent-kind cell/proof run (RAL-102).
On Windows this is [psmux](https://github.com/psmux/psmux), a native
Windows tmux alternative — not real tmux, and not a drop-in (see
`docs/tmux-embedding.md`'s "Why embedding isn't done yet" for the known
`respawn-pane` gap this project already works around).

psmux **3.3.5 and earlier have a real, reproducible bug** where a live tmux
session can silently disappear mid-run — the backing OS process exits (or
becomes unreachable) with no corresponding `kill-session` call, surfacing in
ralphus as `runner produced no result file: The system cannot find the file
specified. (os error 2)`. Root-caused to a race in psmux's internal
pre-warmed-session-pool claiming logic (`__warm__`-prefixed processes you
may see in Task Manager — internal to psmux, not something ralphus creates
directly): concurrent or slow `new-session` claims could lose the session
entirely. Fixed upstream in v3.3.6 (release notes: `fix: prevent live
sessions from disappearing`, `fix: reduce warm-claim session loss under
rapid new-session (atomic claim)`, `fix: eliminate residual warm session
loss (double-create on slow claim)`). See `PSMUX_CRASH_NOTES.local.md` for
the full investigation (not committed — local working notes).

Check your version:

```bash
tmux -V   # must print "tmux 3.3.6" or later
```

Upgrade via winget (the package is `marlocarlo.psmux`, aliased to `tmux`/
`pmux`/`psmux` on `PATH`):

```powershell
winget upgrade --id marlocarlo.psmux
```

If the upgrade fails with an "Access is denied" removing the old
`tmux.exe`, a leftover psmux process (often one of its own `__warm__` pool
processes) is holding the file open — find and stop any `tmux.exe` running
from the old install path, then retry.

Separately: psmux also does not fully release the OS process for a session
it's told to `kill-session` on — the process (and its `conhost.exe` child)
can leak and, left unbounded, accumulate enough to spike CPU. This is a
distinct, still-open issue (see `PSMUX_CRASH_NOTES.local.md`) — a version
bump does not fix it. If a machine's CPU is unexpectedly pegged after heavy
ralphus usage, check for accumulated `tmux.exe`/`conhost.exe` processes with
no corresponding `tmux ls` session and kill them.

## tmux (macOS / Linux)

Real upstream tmux — no known version constraint.
