# Dependencies

Two audiences, two sections: someone **building/testing** this repo needs
section 1; someone **running** the compiled binaries needs section 2. Section
3 covers dependencies gated behind an opt-in feature or mode, documented in
full elsewhere — this is just the index.

**RAL-416 health catalog:** every runtime dependency below that `ralphus
check health` actually validates has a stable entry in
`ralphus_core::health_catalog` -- a catalog id, its daemon/remote
applicability, Free/OnDemand cost tier, and Required/Optional/FallbackOnly
requirement level. Browse it directly with `ralphus check catalog` (no
probes run) rather than cross-referencing this doc by hand; a `catalog id
\`<id>\`` marker next to a dependency's prose below (e.g. `git`, `ripgrep`)
names the exact entry that backs it.

**Platform support:** Linux and Windows are fully supported and get CI's full
fmt/clippy/test suite. macOS is supported as a daemon host on an
**experimental** basis (RAL-398) — it already runs the same `cfg(unix)` code
paths Linux does, and CI runs a build-only macOS leg on every PR, but there
is no real macOS hardware to validate a heavier leg against yet. Everything
below that says "macOS" or "Unix" applies to it the same as Linux unless
called out otherwise.

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

`ralphus check health`'s `git` check (Harness section, catalog id `git` --
RAL-416's `ralphus_core::health_catalog`) only treats a missing binary as a
hard `fail` when at least one registered project actually uses
`vcs = "git"` (or project state can't be determined at all, e.g. the daemon
is unreachable) — RAL-415. A project registered with a non-Git `vcs` kind
skips its own git-repo-validity check too (reported `SKIP`, not `FAIL`),
since only `git` is actually implemented as a `Vcs` adapter today
(`daemon/src/vcs.rs`).

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

**`pi` 0.85.1 or later is the recommended version.** Check your installed
version with `pi --version` and upgrade via whatever channel you installed
it from (e.g. `npm install -g @pi/cli`).

On Windows, the Pi backend supports local worktree paths, including paths in
the `\\?\C:\...` extended-length form. It does not support a worktree hosted
on a genuine UNC share (`\\server\share\...` or `\\?\UNC\server\share\...`):
the npm-provided `pi.cmd` launcher runs through `cmd.exe`, which cannot use a
UNC path as its current directory.

### gh / glab — optional, best-effort only

`daemon/src/forge.rs`'s `resolve_cli_token` shells out to `gh auth token` or
`glab auth status` as a **fallback** token source when a forge's primary
token env var (`[forge].token_env`, e.g. `RALPHUS_GITHUB_TOKEN`/
`RALPHUS_GITLAB_TOKEN`) isn't set — reusing a token the CLI already has
cached from a prior interactive `gh auth login` / `glab auth login`. Neither
binary is required: any failure (not installed, not logged in, unexpected
output) is treated as "no token from this source," not an error.
`ralphus check health`'s Harness section (`cli/src/health.rs`'s `check_gh`/
`check_glab`, RAL-415, catalog id `gh` / catalog id `glab` — RAL-416 classifies both as
`FallbackOnly` requirement level) probes for both on `PATH` and always
reports `pass` either way, explaining this fallback role in the detail text
— a missing `gh`/`glab` never fails `check health`.

### ripgrep (rg) — optional, best-effort only

Agents are prompted (`daemon/src/runner.rs`'s tools system prompt) to prefer
`rg` for shell searches and to reach for `grep` only when it's unavailable,
so a missing ripgrep degrades search speed on large repositories without
breaking anything. Nothing in the compiled binaries invokes `rg` itself.

`ralphus check health`'s Harness section (`cli/src/health.rs`'s
`check_ripgrep`, RAL-522) and the daemon's hourly Free-tier health sweep
(`daemon/src/health_sweep.rs`, which backs the board's Health tab) both run
the same shared probe (`runner/src/ripgrep.rs`) as a single diagnostic
(catalog id `ripgrep`): it resolves `rg` on PATH, invokes `rg --version` on
the resolved executable, and on success reports `<path> (<version>)`. The
finding is a `warn`, never a `fail`, and each failure shape (not on PATH;
resolved but not executable; ran but reported no ripgrep version) carries
its own actionable detail.

Check your version:

```bash
rg --version
```

Install via your package manager (e.g. `winget install BurntSushi.ripgrep.MSVC`,
`brew install ripgrep`, `apt install ripgrep`) — see
<https://github.com/BurntSushi/ripgrep#installation>.

### OS-integration binaries — best-effort, platform-gated

A long tail of small, platform-specific subprocess calls, each with a
graceful fallback — none of these block core functionality if missing:

- **Windows:** `powershell` (per-PID CPU/memory sampling in
  `daemon/src/resources.rs`;
  fallback console in `daemon/src/server.rs`), `wt` (Windows Terminal, for
  `spawn_in_terminal` — falls back to `powershell` if unavailable), `where`
  (PATH resolution helper), `cmd /C` (compound/raw shell-line execution in
  the CLI backends and `run_bash` tool, and to open a path with its OS
  default handler).
- **macOS/Linux:** `sh -c` (the POSIX equivalent of `cmd /C` above), `open`
  (macOS) / `xdg-open` (Linux) for opening a terminal-log snapshot with the
  OS default handler.
- **macOS only:** `ps` (per-PID CPU/memory sampling in
  `daemon/src/resources.rs`, RAL-398 — the macOS equivalent of the
  Windows/Linux readers above; same best-effort/graceful-degradation
  contract).
- **Cross-platform, optional:** `$VISUAL`/`$EDITOR` (user-configured, for
  viewing terminal-log snapshots — falls back to `open`/`xdg-open`/`cmd
  start` if unset), `nvidia-smi` (catalog id `nvidia-smi`; optional GPU
  memory sampling in `daemon/src/resources.rs` — any failure, including "not
  an NVIDIA machine," yields an empty result rather than an error).

### ssh / rsync / tar — only for the SSH machine provider

Not needed for local execution at all. Fully documented in
[`docs/machine-providers.md`](machine-providers.md)'s "The SSH provider
(RAL-200)" section: the provider shells out to real `ssh`, and to `rsync`
when available or a `tar | ssh tar -x` fallback otherwise (Windows ships
OpenSSH + bsdtar but not `rsync`). Registering a machine to use this
provider is an explicit admin action (`POST /api/machines`), never something
a submitted task file can trigger itself.

`--all-remotes` (`ralphus check health --all-remotes`, or the board's Health
tab) checks every configured `[machine.targets.*]` entry against this
provider (`daemon/src/health_targets.rs`) — RAL-416's `ralphus_core::health_catalog`
gives each of those sub-checks its own catalog id (`remote-resolve`,
`remote-ssh-reachable`, `remote-capabilities`, `remote-root`,
`remote-git-version`, `remote-git-identity-name`, `remote-git-identity-email`,
`remote-push-credentials`, `remote-runner`), every one of them `OnDemand`
(never part of the daemon's hourly background sweep — see
`daemon/src/health_sweep.rs`'s module doc comment). `remote-push-credentials`
and `remote-runner` are *documented-unverified*: both always report a `warn`
explaining why no safe, non-mutating check exists yet, rather than a real
pass/fail.

## 3. Optional / conditional dependencies

These only apply if you opt into the corresponding feature or mode — each is
documented in full at its own doc, not duplicated here:

| Feature | Dependency | Doc |
|---|---|---|
| OpenTelemetry tracing | A reachable OTLP endpoint, only if `$OTEL_EXPORTER_OTLP_ENDPOINT` is set; zero network calls otherwise | [`docs/otel-tracing.md`](otel-tracing.md) |
| `--features secure-dist` build | A signed `ralphus.lic` file (or `$RALPHUS_LICENSE` path override) | [`docs/secure-dist.md`](secure-dist.md), `auth/AGENTS.md` |
| Container execution mode | Docker Engine / Docker Desktop | [`docs/container-mode.md`](container-mode.md) |
| SSH machine providers | `ssh`/`rsync`/`tar` on the *daemon host*, an SSH-reachable remote — see the runtime section above | [`docs/machine-providers.md`](machine-providers.md) |

## Agent CLI login checks (`claude-code-login`, `codex-login`)

The daemon's health sweep asks each agent CLI whether it is logged in, using
the same command the runner would spawn (database override, then
`RALPHUS_CLAUDE_COMMAND`/`RALPHUS_CODEX_COMMAND`, then `claude`/`codex`) and the
same config dir the runner copies credentials from (`CLAUDE_CONFIG_DIR`/`CODEX_HOME`,
else `~/.claude`/`~/.codex`). Only the daemon host's login is checked.

| Backend | Probe | Logged in | Logged out |
|---|---|---|---|
| Claude Code | `claude auth status` (JSON; `loggedIn`, `authMethod`) | `pass`; `ANTHROPIC_API_KEY` counts (reported "unverified") | `warn`, run `claude auth login` |
| Codex | `codex login status` (text) | `pass`; `CODEX_API_KEY`/`OPENAI_API_KEY` counts when status says logged out (reported "unverified") | `warn`, run `codex login` |

Both probes are offline: they detect never-logged-in, not an expired or revoked
token. A missing binary or compound command is `skip`; a timeout (5s) or an
older CLI without the status subcommand is `warn`. The account email is never
included, only auth method and organization. Same behavior on Windows, Linux
and macOS, with one gap: on macOS Claude Code keeps credentials in the Keychain,
which `claude auth status` reads but per-cell isolation (`.credentials.json`
copy) does not, so a pass can overstate what an isolated cell sees; the detail
carries a macOS caveat.
