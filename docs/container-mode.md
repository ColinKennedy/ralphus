# Container execution mode (RAL-225)

The real `claude`/`codex` CLIs are launched with full permission-bypass flags
(`--dangerously-skip-permissions` in `cli/src/ralphus/runner/claude_code_backend.py`,
`--dangerously-bypass-approvals-and-sandbox` in
`cli/src/ralphus/runner/codex_backend.py`) — full tool access, bounded only by
whatever `cwd` and prompt they're given. Validating a cell's `cwd` string
(RAL-224) bounds where an agent *starts*; it does not bound what the resulting
process can *reach* once running — `cd ..`, an absolute path in a tool call, a
symlink followed outside the workspace are all still just OS-permitted file
operations from the process's point of view. This document covers the other
half: real OS-level confinement, so a subprocess is blocked from reaching
outside its intended workspace by the operating system itself.

## The shape adopted

A sibling project's actual pattern for this problem is a coarser
**container-per-workspace** model, not per-agent sandboxing. Container mode
adopts the same shape: the daemon, the librarian, and the Python runner (and
therefore every locally-executed agent subprocess the runner spawns) run
inside **one hardened container**, with the container's filesystem view
confined to a single bind-mounted workspace root.

This is an **available, configurable execution mode** — not forced on every
setup:

| Mode | How you run it | What's confined |
|---|---|---|
| Bare subprocess (default) | `scripts/build-debug.sh` / `scripts/build-release.*` / `scripts/start-ralphus.*` | Nothing beyond normal OS file permissions — the daemon, runner, and every agent subprocess run as your own user, with your own filesystem access. |
| Container | `scripts/run-container.sh` / `scripts/run-container.cmd` (wraps `docker compose -f docker/docker-compose.yml`) | Everything under the container runs inside one Linux container whose filesystem is, by construction, limited to the one host directory you bind-mount in plus its own internal state. |

Selecting a mode is choosing which script to run — the same way
`build-debug.sh` (fast, from source) and `build-release.sh` (standalone
`dist/` binaries) are already two selectable ways to run this project.

**Explicitly out of scope for this task:** true per-task/per-agent
confinement — a separate sandbox for each cell rather than one shared
container for the whole daemon instance. Every cell/task/squad this daemon
instance schedules shares the same container and the same mounted
`/workspaces` — one cell cannot be confined to a *narrower* directory than
another's within the same daemon instance. See "What this does NOT confine"
below, and "Future follow-up" at the end.

## Running it

```bash
# bash / macOS / Linux
RALPHUS_WORKSPACE_ROOT=/path/to/your/checkouts bash scripts/run-container.sh

# Windows (cmd)
set RALPHUS_WORKSPACE_ROOT=C:\path\to\your\checkouts
scripts\run-container.cmd
```

`RALPHUS_WORKSPACE_ROOT` is the **only** host directory the container can
read or write. It is bind-mounted read-write at `/workspaces` inside the
container — point it at a parent directory of whatever project checkouts your
task files' `cwd`s live under, and use the container-side path (e.g.
`/workspaces/my-project`) as `cwd` in task files you submit against this
instance. The daemon's own SQLite DB / terminal logs / Cartographer live in a
separate named Docker volume (`ralphus-state`, mounted at
`/home/ralphus/.ralphus`) — persisted across container restarts, but never
exposed to an agent cell's `cwd` the way `/workspaces` is.

The daemon's HTTP API and the librarian board are published on the same ports
as bare-subprocess mode (7890 / 7474 by default; override with
`--daemon-port`/`--librarian-port`), so `ralphus submit`/`ralphus status`/a
browser pointed at `http://127.0.0.1:7474` work identically regardless of
which mode is running underneath.

### Why the daemon needed a code change to run in a container

The daemon and librarian each hardcoded a `127.0.0.1` bind address. Docker's
published-port mapping (`-p 7890:7890`) routes to the container's *external*
network interface, not its loopback — a listener bound only to `127.0.0.1`
inside the container is unreachable from the host despite the port mapping.
Both binaries now read `RALPHUS_BIND_ADDR` (see
`ralphus_daemon::resolve_bind_host` / `ralphus_librarian::resolve_bind_host`)
and bind that host instead, defaulting to `127.0.0.1` when unset. The image
sets `RALPHUS_BIND_ADDR=0.0.0.0`; nothing in bare-subprocess mode sets this
variable, so its behavior — and its default, security-relevant `127.0.0.1`
binding — is completely unchanged.

## What this DOES confine

Enforced by the container boundary itself (Docker's mount namespace + the
hardening flags in `docker/docker-compose.yml`), not by any application-level
check:

- **Filesystem reads/writes.** The container's root filesystem is mounted
  `read_only: true`; the only writable paths inside it are `/workspaces` (the
  one host directory you chose) and `/home/ralphus/.ralphus` (daemon state),
  plus a `tmpfs` at `/tmp`. An agent subprocess — however it's prompted,
  however it tries to `cd`, follow a symlink, or pass an absolute path to a
  tool call — cannot read or write anything on the host outside
  `RALPHUS_WORKSPACE_ROOT`, because nothing else on the host is visible
  inside the container's mount namespace at all.
- **Privilege escalation.** `cap_drop: [ALL]` and `security_opt:
  [no-new-privileges:true]` remove every Linux capability beyond what an
  unprivileged process needs, and block a subprocess from gaining more
  privileges than its parent (e.g. via a setuid binary) even if one were
  present.
- **Running as root.** The image's `USER ralphus` (uid 10001, no login shell)
  means every process in the container — daemon, librarian, runner, every
  spawned agent CLI — runs unprivileged, so even a successful escape attempt
  still hits normal Unix file permissions on the way out, not root access.

## What this does NOT confine (documented gap, not silently partial)

This mode closes the *filesystem* half of the problem. It does **not**:

- **Restrict network access.** An agent subprocess inside the container can
  still make outbound network calls (fetch a URL, exfiltrate data over HTTP,
  reach an internal service) exactly as it could on the bare host — nothing
  in this task adds an egress firewall or network namespace restriction.
  If that matters for your threat model, add your own network policy (e.g. a
  Docker network with no default route, or an explicit firewall rule) on top
  of this compose file; it is not built in.
- **Confine one cell more narrowly than another.** Every cell this
  daemon instance runs shares the same container and the same
  `/workspaces` mount. A task whose `cwd` is
  `/workspaces/project-a` can still reach `/workspaces/project-b` if both
  happen to live under the same `RALPHUS_WORKSPACE_ROOT` — the boundary is
  per-daemon-instance, not per-task or per-agent. Run a separate daemon
  instance (separate container, separate `RALPHUS_WORKSPACE_ROOT`, separate
  port/DB per RAL-164's multi-instance pattern) if two workloads must not
  share a filesystem view.
- **Confine the SSH machine-provider path.** `ralphus-ssh-provider` /
  `ssh-provider/` is untouched by this task by design — remote execution
  there already gets a real isolation boundary for free (a separate machine).
  A task routed to a `machine = "ssh:..."` target runs on that remote host
  exactly as before, regardless of which mode the daemon itself runs in.

## tmux-wrapped live cells still work unchanged

Agent-kind cell/proof runs execute inside a detached tmux session
(`daemon/src/tmux.rs`), which is what makes "Show Live View" and "Open
Terminal Log" possible. Container mode installs real Linux tmux (not the
Windows `psmux` alternative this project also supports) — `daemon/src/tmux.rs`
resolves it the same way it always does (`RALPHUS_TMUX_CMD` override, else
`tmux` on `PATH`), so no daemon-side code path changes.

Verified manually, against the same running container as above:

```bash
$ docker compose -f docker/docker-compose.yml exec ralphus sh -c \
    'tmux new-session -d -s live -x 80 -y 24 "echo hello-from-pane; sleep 60"; sleep 1; tmux list-sessions; tmux capture-pane -t live -p'
live: 1 windows (created ...)
hello-from-pane
```

A detached tmux session starts, keeps running, and `capture-pane` (the same
primitive the board's Live View polls) reads its output — the same mechanism
a real agent cell or `docker compose exec ralphus tmux attach -t
<session-name>` (interactive terminal attach) relies on.

## Manual verification (filesystem confinement)

Automated: this repo's CI (`.github/workflows/ci.yml`) does not run a Docker
job — adding one is out of scope for this task. Instead, `docker/Dockerfile`
was actually built and run against a real Docker Engine (Docker Desktop,
Windows/WSL2 backend) and exercised by hand. Transcript below, lightly
trimmed; every command was run against a live container with
`RALPHUS_WORKSPACE_ROOT` pointed at a throwaway host directory containing
exactly one file (`marker.txt`):

```bash
$ mkdir -p /tmp/ralphus-workspaces && echo "sentinel-inside-workspace" > /tmp/ralphus-workspaces/marker.txt
$ RALPHUS_WORKSPACE_ROOT=/tmp/ralphus-workspaces bash scripts/run-container.sh   # (or docker compose -f docker/docker-compose.yml up -d)

# -- 1. the mounted workspace IS reachable, proving this isn't "everything fails" --
$ docker compose -f docker/docker-compose.yml exec ralphus cat /workspaces/marker.txt
sentinel-inside-workspace

# -- 2. nothing from the host repo checkout (or anywhere else on the host) is
#       visible ANYWHERE in the container, because it was never mounted --
$ docker compose -f docker/docker-compose.yml exec ralphus sh -c \
    'find / -xdev -maxdepth 2 -iname "AGENTS.md" -o -iname "Cargo.toml"'
(no output -- nothing found)

# -- 3. the read-only rootfs blocks writes anywhere outside the two
#       writable mounts, even to paths that visibly "exist" in the image --
$ docker compose -f docker/docker-compose.yml exec ralphus sh -c 'echo pwned > /usr/local/bin/pwned'
sh: 1: cannot create /usr/local/bin/pwned: Read-only file system
$ docker compose -f docker/docker-compose.yml exec ralphus sh -c 'echo pwned > /etc/pwned'
sh: 1: cannot create /etc/pwned: Read-only file system

# -- 4. non-root, zero effective capabilities --
$ docker compose -f docker/docker-compose.yml exec ralphus sh -c 'id; cat /proc/1/status | grep CapEff'
uid=10001(ralphus) gid=10001(ralphus) groups=10001(ralphus)
CapEff:	0000000000000000

# -- 5. the exact vectors the ticket calls out: `cd ..` and a symlink out of
#       the workspace. Both still only ever reach the CONTAINER's own /etc
#       (this image's own passwd file, not the host's) -- never the host,
#       because the host is not part of this mount namespace at all --
$ docker compose -f docker/docker-compose.yml exec ralphus sh -c \
    'ln -sf /etc/passwd /workspaces/escape-link; head -c 60 /workspaces/escape-link'
root:x:0:0:root:/root:/bin/bash
daemon:x:1:1:daemon:/usr/sbi[...]

# -- 6. a write INSIDE the workspace really does land on the host --
$ docker compose -f docker/docker-compose.yml exec ralphus sh -c 'echo from-container > /workspaces/proof.txt'
$ cat /tmp/ralphus-workspaces/proof.txt
from-container

# -- 7. the daemon/librarian are reachable through the published ports
#       (proves the RALPHUS_BIND_ADDR=0.0.0.0 fix actually works end to end) --
$ curl -s http://127.0.0.1:7890/api/daemon
{"name":"ralphus-daemon","version":"0.1.0","status":"ok","db":"ok", ...}
$ curl -s -o /dev/null -w "%{http_code}\n" http://127.0.0.1:7474/
200
```

Step 2's empty result is the core of the confinement: there is no filesystem
operation an agent subprocess can perform — however it is prompted, `cd ..`,
an absolute path, or a symlink — that reaches anything on the host outside
`RALPHUS_WORKSPACE_ROOT`, because nothing else on the host is part of the
container's mount namespace in the first place. This is enforced by the
Linux kernel (bind mounts + `read_only` rootfs), not by anything in
ralphus's own code, which is what makes it real confinement rather than a
convention an injected prompt could talk an agent out of.

**A real bug this verification caught:** the first build set the container's
`ralphus` user's login shell to `/usr/sbin/nologin`, intending it as
additional hardening. That silently broke every tmux pane — tmux resolves
the pane's interpreter from the user's passwd shell even when given an
explicit command, and `nologin` "succeeds" as an execve() target by printing
`This account is currently not available.` and exiting immediately, so every
pane died on creation. `nologin` was only ever relevant to blocking
interactive login (SSH/console) on this uid, which was never how it's used —
this container only ever spawns processes as this uid, never logs into it.
Fixed to `/bin/bash` in `docker/Dockerfile`; see the manual tmux check below
for the passing re-verification. This is exactly the kind of gap that stays
invisible without actually running the container and exercising the tmux
path, not just reading the compose file's flags.

## Future follow-up (explicitly out of scope here)

Per-task/per-agent OS-level confinement — a separate sandbox boundary for
*each cell*, rather than one shared container per daemon instance — was
explicitly descoped for this task (see the ticket's "Resolved decision"). If
that's ever needed: the shape most consistent with this codebase would be a
new machine provider (see `docs/machine-providers.md`) that provisions a
short-lived container (or a Linux namespace sandbox, `bubblewrap`-style) per
cell rather than per daemon instance — the provider contract already
exists and already isolates "this cell's work" from "the daemon's own
process" for the SSH provider today, just with a real remote machine as the
isolation boundary instead of a local sandbox.
