# scripts/

Two build scripts, two purposes:

| Script | Speed | Output | Use it to |
|---|---|---|---|
| `build-debug.sh` | seconds (incremental) | runs from source, no `dist/` | iterate — esp. the GUI |
| `build-release.sh` | minutes | four standalone exes in `dist/` | package / distribute |

**Fast dev loop — `bash scripts/build-debug.sh`.** Debug-builds all four binaries (daemon, librarian, runner, CLI) with `cargo` (incremental, ~seconds each) and points `RALPHUS_RUNNER_CMD` at the just-built debug `ralphus-runner` exe. It boots the daemon (`127.0.0.1:7890`) in the background and the librarian board (`127.0.0.1:7474`) in the foreground; **Ctrl-C stops both**. Invoke the CLI yourself from another shell, e.g. `target/debug/ralphus status`.

**Release build — `bash scripts/build-release.sh`.** Builds copyable standalone binaries into `dist/`: `ralphus-daemon`, `ralphus-librarian`, `ralphus` (CLI), `ralphus-runner` — all four are a single `cargo build --release` now, no PyInstaller step. Stop any running daemon/librarian/CLI/runner first: they lock their own `dist/` exes and the copy step will fail with "Device or resource busy".

**Testing ralphus using ralphus (multi-instance dev stacks, RAL-164).** `ralphus-daemon serve` accepts `--db <path>` alongside `--port`, and `build-debug.sh`/`.cmd` accept a matching `--db-path`. To keep your regular ralphus instance open while exercising a change in another git worktree, give that worktree's stack its own port *and* its own DB explicitly:

```bash
# worktree A (your regular instance) — unchanged, defaults
bash scripts/build-debug.sh

# worktree B — fully isolated second stack
bash scripts/build-debug.sh --daemon-port 7891 --librarian-port 7475 --db-path ~/.ralphus/tasks-worktree-b.db
```

This is deliberately explicit, not auto-picked: if a script silently chose a port or DB path on `start`, a later `ralphus-daemon stop --port N` (a new shell, a different agent) would have no reliable way to know what to target. Passing a non-default `--daemon-port` without `--db-path` still gets automatic DB isolation (derived as `~/.ralphus/tasks-<port>.db`) — only the *default* port keeps using the plain `~/.ralphus/tasks.db` it always has, so existing setups are unaffected. Point the CLI or a browser at the second instance with `ralphus --daemon-url http://127.0.0.1:7891 ...` / `http://127.0.0.1:7475`, and stop it with `ralphus-daemon stop --port 7891` when done.

**Live-testing a checkout — `bash scripts/live-stack.sh start|stop|env|plan [--name NAME]`** (on Windows, `scripts\live-stack.cmd` with the same arguments; the two share one `stack.env` format, so either can stop a stack the other started).
The agent-safe version of the multi-instance recipe above: it builds the checkout
it lives in, picks free ports from 17890/17474 (never 7890/7474), gives the stack
its own DB under `~/.ralphus/live-stacks/<name>/`, launches the daemon and board
detached, and records their pids in `stack.env` (`DAEMON_URL`, `LIBRARIAN_URL`,
`CLI`, ...). `stop` kills exactly the recorded pids; `plan` prints what `start`
would choose without building or launching anything. `NAME` defaults to the
checkout's directory name, so each worktree has its own. It is what live actions
use to get a running stack from inside a ralphus cell (see
[`.agent/agent-conduct.md`](../.agent/agent-conduct.md)). It shares `~/.ralphus`
(token, logs) with the host stack but not its database.

Run the pieces directly:

```bash
ralphus-daemon serve                          # HTTP API on 127.0.0.1:7890 (+ scheduler)
ralphus-librarian serve [--port 7474]         # web board; RALPHUS_DAEMON_URL points it at the daemon
ralphus submit task.toml                      # or: validate / status / review / ...
```

`scripts/build-release.cmd` (Windows) builds all four standalone executables into `.\dist` via a single `cargo build --release --package ralphus-daemon --package ralphus-librarian --package ralphus-cli --package ralphus-runner`.

**Container execution mode (RAL-225).** A third, opt-in way to run the stack: `scripts/run-container.sh` / `scripts/run-container.cmd` runs the daemon + librarian + runner inside one hardened Linux container (`docker/Dockerfile` + `docker/docker-compose.yml`), with the container's filesystem confined to a single bind-mounted `RALPHUS_WORKSPACE_ROOT` — so every locally-executed agent subprocess is blocked from reaching anything else on the host, enforced by the OS itself rather than by prompt discipline. Bare-subprocess (the two build scripts above) remains the default and is untouched by this mode's existence. See [`docs/container-mode.md`](../docs/container-mode.md) for the full rationale, exactly what is and isn't confined, and a manual escape-attempt verification procedure. The SSH machine-provider path (`ssh-provider/`) is unrelated and unaffected — it already gets isolation for free from running on a separate machine.

**Docker SSH target fixture.** `scripts/ssh-target.ps1` (Windows) and
`scripts/ssh-target.sh` (bash) manage a separate Linux/OpenSSH container that
the host daemon can treat as a real remote machine. It never starts or stops a
Ralphus daemon. Its project/job root, test bare Git origin, and host key use
named Docker volumes; `down` preserves them and `destroy` removes them. See
[`docs/remote-docker-target.md`](../docs/remote-docker-target.md).

**Guided exercises — `bash scripts/check-initialize-exercises.sh`.** Runs every
`ralphus initialize <exercise>` (machine, mailbox, triage, review, waypoint)
live, locally and with `--remote` (every cell and review on the loopback
machine provider in strict mode), plus the exempt `initialize followup`
locally only, each against its own throwaway daemon that
`--stop` shuts down. Build `ralphus`, `ralphus-daemon` and `ralphus-runner`
first; needs git and Python 3. `--only review,waypoint`, `--local-only` /
`--remote-only` narrow it. CI's `initialize-exercises` job runs it. It never touches a running
dev stack: each exercise gets its own port, database, and home.
The rules every exercise must meet (no LLM, local and remote, CI-wired) live in
[`cli/src/commands/initialize/AGENTS.md`](../cli/src/commands/initialize/AGENTS.md).

`followup` (the deferred-prophecy follow-up flow) is agent-less: its cells run
a `raw` agent profile whose executable is a renamed copy of the `ralphus`
binary (`initialize/followup.rs`), and a review's merge is a git fast-forward
the daemon notices through its already-in-base check, standing in for a forge
merge. Everything else -- daemon, scheduler, runner, prophecy and waypoint
stores, squad submission -- is real. It is local only: the review merge it
drives is a git fast-forward in the daemon's own checkout. Because of both,
it is a documented exemption from the exercise rules, not one of `EXERCISES`.

**Temp sweep — `pwsh scripts/sweep-temp.ps1 [-DryRun] [-OlderThanHours 24] [-Root <dir>] [-IncludeThirdParty]`.**
Windows-only one-shot cleanup of stale ralphus test leftovers in `%TEMP%` and
`%LOCALAPPDATA%\ralphus\test-tmp` (a large `%TEMP%` stalls every logon; see
`.agent/gotchas.md`). Allow-list only; never `Temp\claude`, `Temp\ralphus`, or
anything modified in the last 2 h; prints failures and `deleted=N failed=M`.
Run `-DryRun` first; exercise it only on a scratch `-Root`.

**Documentation site.** `scripts/docs-build.sh` / `.cmd` renders
`docs/site/pages/*.md` into `docs/site/_site/` via MkDocs + Material —
fast, no Playwright, safe to re-run on every doc edit.
`scripts/docs-screenshots.sh` / `.cmd` is the separate, heavier step that
regenerates the committed PNGs under `docs/site/pages/screenshots/` from the
real compiled `ralphus-librarian` — only needed after a board UI change that
can't be previewed in dev mode (the dev librarian serves `librarian/assets`
from disk, so chunk/CSS/shell edits need no rebuild at all). See [`docs/docs-site.md`](../docs/docs-site.md) for the full
breakdown of both, plus the separate `ralphus-docs-helpmap` command that
regenerates `docs/cli-reference.md`'s help-map block.

**`dist/` layout — four standalone exes, no sibling directories:**

```
dist/
  ralphus-daemon.exe
  ralphus-librarian.exe
  ralphus.exe
  ralphus-runner.exe
```

Put `dist/` on PATH to get the `ralphus` CLI and have `RALPHUS_RUNNER_CMD` resolve `ralphus-runner` automatically, or point `RALPHUS_RUNNER_CMD` at the full path to `dist/ralphus-runner.exe` explicitly.

**History (pre rust-port):** the CLI and runner used to be PyInstaller `--onedir` bundles (`dist/ralphus/ralphus.exe` + `_internal/`, `dist/ralphus-runner/ralphus-runner.exe` + `_internal/`), which themselves replaced an earlier `--onefile` build after its bootloader's cache validation failed with `Security validation failure: parent process has different executable!` on two machines launched from a sandboxed/reparenting tool layer (see `PERMISSIONS_ISSUE.local.md`). None of that applies anymore — see "The Rust CLI/runner port" in the root `AGENTS.md`.
