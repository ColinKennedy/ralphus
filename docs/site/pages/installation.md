# Installation

## Prerequisites

- **Rust** (stable toolchain) — builds the daemon and web board.
- **[uv](https://docs.astral.sh/uv/)** — manages the Python CLI/runner
  environment.
- **Ollama** (optional) — only needed if you plan to run local models instead
  of a cloud model like Claude.

## Fast local dev loop

Clone the repository, then from its root:

```bash
bash scripts/build-debug.sh
```

This debug-builds the daemon and web board with `cargo` (incremental — a
few seconds after the first build), points the daemon at the Python runner
venv (`uv sync --extra runner` under the hood, so no slow one-file bundling),
starts the daemon on `127.0.0.1:7890` in the background, and starts the web
board on `127.0.0.1:7474` in the foreground. Open
[`http://127.0.0.1:7474`](http://127.0.0.1:7474) and `Ctrl-C` stops both
processes.

This is the loop to use while iterating — including on the board's own UI:
edit `librarian/assets/board.html`, re-run the script, refresh the browser.

## Standalone binaries

To produce copyable, distributable executables instead (slower — several
minutes, since it bundles the Python runner with PyInstaller):

```bash
bash scripts/build-release.sh      # macOS/Linux
scripts\build-release.cmd          # Windows
```

This produces four executables in `dist/`: `ralphus-daemon`,
`ralphus-librarian`, `ralphus` (the CLI), and `ralphus-runner`. Stop any
already-running daemon/librarian first — they lock their own `dist/`
executables.

## Running the pieces directly

Once built (either path above):

```bash
ralphus-daemon serve                          # HTTP API on 127.0.0.1:7890
ralphus-librarian serve [--port 7474]         # the web board
ralphus submit task.toml                      # submit a task file
ralphus check health                          # check daemon/git/runner health
```

`ralphus check health` is worth running first on a new machine — it checks
that the daemon is reachable, `git` is on `PATH`, the runner is found, and
(if you're using local models) that Ollama is reachable.

## Next

Head to the [Overview](overview.md) for the concepts, or straight to the
[Tasks](views/tasks.md) tab tour to see what the board looks like once
something is running.
