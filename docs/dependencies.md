# Dependencies

Two audiences, two sections: someone **building/testing** this repo needs
section 1; someone **running** the compiled binaries needs section 2. Section
3 covers dependencies gated behind an opt-in feature or mode, documented in
full elsewhere — this is just the index.

## 1. Build-time / installation dependencies

What you need installed to build and test the workspace. Nothing in this
section is needed by an end user of the compiled `dist/` binaries.

| Dependency | Version | Why | Notes |
|---|---|---|---|
| Rust toolchain | `rust-version = "1.85"` (root `Cargo.toml`), but CI's pinned MSRV build-only leg tests **1.86** (`.github/workflows/ci.yml`) | Compiles all 11 workspace crates | The two numbers disagree; treat **1.86** as the practical minimum until reconciled. `stable` is what CI's full fmt/clippy/test suite actually runs against on both Linux and Windows. No `rust-toolchain.toml` pins a toolchain — `rustup` resolves whatever `stable`/pinned version CI requests. |
| A C compiler (`cc`, MSVC on Windows) | whatever `cc-rs` needs | `rusqlite`'s `bundled` feature (set once in `[workspace.dependencies]`, inherited by every crate that uses it) compiles SQLite from bundled C source at build time | No system-installed SQLite is required — this is the only reason a C toolchain is needed. Windows: the MSVC toolchain that ships with `rustup`'s `stable-x86_64-pc-windows-msvc` already includes this; no separate install. |
| Node.js 22+ | pinned in CI (`.github/workflows/ci.yml`) | Lints/type-checks/tests the librarian's board chunk JS (`npm run lint` / `typecheck` / `knip` / `test`) | Dev-only — see `package.json`'s own description: "Not a runtime dependency — the librarian embeds the board assets via include_str! and ships no Node code." No `.nvmrc` at repo root. |
| Python 3.11+, managed via [uv](https://docs.astral.sh/uv/) | `requires-python = ">=3.11"` (`cli/pyproject.toml`) | `cli/` is **not shipped** — see root `AGENTS.md`'s Architecture table. It exists only for `docsgen/` (doc screenshot generation via the `docs` optional extra: `playwright`, `mkdocs`, `mkdocs-material`) and a trimmed `bench/` graph renderer (`ralphus-bench-graph`, which renders SVG/HTML from JSON the Rust bench harness already wrote — it doesn't run or time anything itself). | The base package has **zero runtime dependencies** (`dependencies = []` in `cli/pyproject.toml`) — everything above is opt-in via `uv sync --extra docs` or the dev dependency group. Do not confuse this with the shipped CLI, which is the Rust `ralphus-cli` crate (binary name `ralphus`) — `cli/` and `cli/` are different things despite the similar name; see `.agent/cli-runner-port.md`. See [`docs/docs-site.md`](docs-site.md) for how these pieces build the `docs/site/` HTML documentation. |

Windows/Unix-only build inputs worth knowing about but not separately
installable: `daemon/Cargo.toml`'s Windows-only `embed-resource` (build-dep,
embeds a `.exe` resource — uses the resource compiler bundled with the
MSVC/rustup toolchain, nothing extra to install) and Unix-only `nix`
(wraps libc for process-group signals, no extra system package).

No system OpenSSL/native-tls is required anywhere in the workspace: every
crate that needs TLS (`ureq` in daemon/runner/cli) uses `rustls`
(pure-Rust), confirmed via `Cargo.lock`. No `git2`/`libgit2-sys` is linked
anywhere either — every git operation shells out to the `git` binary (see
below), never a linked library.

## 2. Runtime dependencies

Tools the **compiled** daemon/runner/CLI expect to find on `PATH` (or via an
explicit override env var) once running — not build-time Rust crate
dependencies.

### git — hard requirement, no override

Every git operation in this codebase shells out to a real `git` binary via
`std::process::Command::new("git")` — there is no linked git library
(`git2`/`libgit2-sys`/`gitoxide` are absent from `Cargo.lock`) and no env var
to point at an alternate binary name or path. `git` must be resolvable on
`PATH` for anything that touches version control: worktrees
(`daemon/src/worktrees.rs`), Guardian's stacked-rebase merge logic
(`daemon/src/guardian_merge.rs`), PR/branch derivation (`daemon/src/pr.rs`,
`daemon/src/forge.rs`), status/diff (`daemon/src/vcs.rs`), and
`ralphus check health`'s repo-detection probe (`cli/src/health.rs`). No
version constraint is currently known to matter.

### tmux / psmux

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

**tmux (macOS / Linux)** — real upstream tmux, no known version constraint.

### Agent backend CLIs — some are subprocesses, some are HTTP, know which is which

`core/src/schema.rs`'s `RESERVED_AGENT_NAMES` lists every built-in agent
backend a task's `agent =` field can name. They split into two very
different runtime shapes:

| Agent name(s) | Runtime shape | What must be on `PATH` |
|---|---|---|
| `claude-code` / `claude-cli` | Spawns an external CLI (`runner/src/claude_code_backend.rs`) | `claude` binary (override via `--executable`/`program_override`) |
| `codex` / `codex-cli` | Spawns an external CLI (`runner/src/codex_backend.rs`) | `codex` binary |
| `pi` | Spawns an external CLI (`runner/src/pi_backend.rs`) | `pi` binary |
| `raw` | Spawns whatever executable the task names | Whatever `--executable` names — no default |
| `claude` / `anthropic` | Hand-rolled tool loop over HTTP (`runner/src/providers.rs`, `runner/src/llm_client.rs`) — **no subprocess** | Nothing on `PATH`; needs a reachable Anthropic API endpoint + credentials |
| `ollama` | Hand-rolled tool loop over HTTP — **no subprocess** | Nothing on `PATH`; needs a reachable Ollama server, default `http://localhost:11434/v1`, override via `$RALPHUS_OLLAMA_URL` |

Only pick a `claude-code`/`codex`/`pi`/`raw` agent if the corresponding CLI is
actually installed; `claude`/`anthropic`/`ollama` never need anything
installed beyond network access to the model.

### gh / glab — optional, best-effort only

`daemon/src/forge.rs`'s `resolve_cli_token` shells out to `gh auth token` or
`glab auth status` as a **fallback** token source when a forge's primary
token env var (`[forge].token_env`, e.g. `RALPHUS_GITHUB_TOKEN`/
`RALPHUS_GITLAB_TOKEN`) isn't set — reusing a token the CLI already has
cached from a prior interactive `gh auth login` / `glab auth login`. Neither
binary is required: any failure (not installed, not logged in, unexpected
output) is treated as "no token from this source," not an error.

### OS-integration binaries — best-effort, platform-gated

A long tail of small, platform-specific subprocess calls, each with a
graceful fallback — none of these block core functionality if missing:

- **Windows:** `powershell` (per-PID CPU/memory sampling in
  `daemon/src/resources.rs`; process liveness checks in `daemon/src/tmux.rs`;
  fallback console in `daemon/src/server.rs`), `wt` (Windows Terminal, for
  `spawn_in_terminal` — falls back to `powershell` if unavailable), `where`
  (PATH resolution helper), `cmd /C` (compound/raw shell-line execution in
  the CLI backends and `run_bash` tool, and to open a path with its OS
  default handler).
- **macOS/Linux:** `sh -c` (the POSIX equivalent of `cmd /C` above), `open`
  (macOS) / `xdg-open` (Linux) for opening a terminal-log snapshot with the
  OS default handler.
- **Cross-platform, optional:** `$VISUAL`/`$EDITOR` (user-configured, for
  viewing terminal-log snapshots — falls back to `open`/`xdg-open`/`cmd
  start` if unset), `nvidia-smi` (optional GPU memory sampling in
  `daemon/src/resources.rs` — any failure, including "not an NVIDIA
  machine," yields an empty result rather than an error).

### ssh / rsync / tar — only for the SSH machine provider

Not needed for local execution at all. Fully documented in
[`docs/machine-providers.md`](machine-providers.md)'s "The SSH provider
(RAL-200)" section: the provider shells out to real `ssh`, and to `rsync`
when available or a `tar | ssh tar -x` fallback otherwise (Windows ships
OpenSSH + bsdtar but not `rsync`). Registering a machine to use this
provider is an explicit admin action (`POST /api/machines`), never something
a submitted task file can trigger itself.

## 3. Optional / conditional dependencies

These only apply if you opt into the corresponding feature or mode — each is
documented in full at its own doc, not duplicated here:

| Feature | Dependency | Doc |
|---|---|---|
| OpenTelemetry tracing | A reachable OTLP endpoint, only if `$OTEL_EXPORTER_OTLP_ENDPOINT` is set; zero network calls otherwise | [`docs/otel-tracing.md`](otel-tracing.md) |
| `--features secure-dist` build | A signed `ralphus.lic` file (or `$RALPHUS_LICENSE` path override) | [`docs/secure-dist.md`](secure-dist.md), `auth/AGENTS.md` |
| Container execution mode | Docker Engine / Docker Desktop | [`docs/container-mode.md`](container-mode.md) |
| SSH machine providers | `ssh`/`rsync`/`tar` on the *daemon host*, an SSH-reachable remote — see the runtime section above | [`docs/machine-providers.md`](machine-providers.md) |
