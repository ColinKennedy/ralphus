# Docker SSH target fixture

This fixture runs a Linux/OpenSSH machine target in Docker while the Ralphus
daemon remains on the host. It exercises the real SSH provider boundary and is
separate from [container mode](container-mode.md), where the daemon and runner
live together inside one container. For running the work in a container *on*
the remote host, see the [container-backed machine fixture](#container-backed-machine-fixture)
below.

The target contains:

- an unprivileged `ralphus` SSH account;
- the current Rust `ralphus-runner` build;
- Git and a seeded bare repository at
  `file:///srv/git/ralphus-test.git`;
- a deterministic mock `claude` executable;
- a persistent remote root at
  `/home/ralphus/.ralphus/remote-work`.

No real Git or agent credentials are included. The setup scripts generate a
fixture-only SSH key under the ignored `.docker-ssh-target` directory. The
container's host key, remote root, and bare origin use named Docker volumes.

## Start on Windows

From a PowerShell prompt at the repository root:

```powershell
.\scripts\ssh-target.ps1 up
```

The command builds the target, waits for it to become healthy, pins its
persistent host key, writes an isolated OpenSSH client configuration, and
prints the provider registration command. It does not modify the user's normal
SSH configuration or start/stop a Ralphus daemon.

Verify direct access:

```powershell
ssh -F .\.docker-ssh-target\ssh_config ralphus-docker -- `
  'whoami && ralphus-runner --version && claude --version && git --version'
```

## Point an isolated daemon/DB at the fixture

Exercising this fixture registers a machine provider and a test project --
state you almost never want mixed into your regular daemon's database while
you are still iterating on remote-execution code. Give the fixture its own
daemon instance and DB rather than reusing your normal one (RAL-164, see
`scripts/AGENTS.md`'s "Testing ralphus using ralphus" section for the general
mechanism this reuses):

```powershell
bash scripts/build-debug.sh --daemon-port 7891 --librarian-port 7475 --db-path ~/.ralphus/tasks-docker-ssh-target.db
```

`--db-path` is optional once you pick a non-default `--daemon-port` -- a
non-default port alone already derives an isolated DB
(`~/.ralphus/tasks-<port>.db`); pass it explicitly only when you want a
memorable name, as above. This does not touch your regular daemon (which stays
on its default port/DB) and does not require stopping it. Point every command
below at the isolated instance:

```powershell
ralphus --daemon-url http://127.0.0.1:7891 machine register ...
ralphus --daemon-url http://127.0.0.1:7891 project git --path ... --name ... --url ...
ralphus --daemon-url http://127.0.0.1:7891 submit some-task.toml
```

Or export it once per shell instead of repeating the flag:

```powershell
$env:RALPHUS_DAEMON_URL = 'http://127.0.0.1:7891'
```

Stop the isolated instance when done, leaving your regular one untouched:

```powershell
ralphus-daemon stop --port 7891
```

## Register the provider

Build the host-side provider:

```powershell
cargo build -p ralphus-ssh-provider
```

Register it with the isolated development daemon from the previous section,
substituting the absolute paths printed by the setup script:

```powershell
ralphus machine register `
  --scheme ssh `
  --program C:\path\to\ralphus\target\debug\ralphus-ssh-provider.exe `
  --arg=--ssh-config `
  --arg C:\path\to\ralphus\.docker-ssh-target\ssh_config `
  --description 'Docker SSH target fixture'
```

Task files address the target as:

```toml
machine = "ssh:ralphus-docker"
```

The provider-level `--ssh-config` argument is stored in the daemon's provider
registry, so the daemon does not need the fixture key or alias in its ordinary
OpenSSH configuration.

Project URL registration and provider-managed persistent Git provisioning are
tracked in `REMOTE_IMPROVEMENTS.local.md`. Until those phases land, the current
SSH `exec` implementation copies a non-Git source tree into a deterministic
directory beneath the remote root.

## Run the real-boundary integration test

```powershell
$env:RALPHUS_SSH_DOCKER_TEST = '1'
$env:RALPHUS_SSH_CONFIG_FILE = `
  (Resolve-Path .\.docker-ssh-target\ssh_config).Path
cargo nextest run -p ralphus-ssh-provider `
  --test docker_ssh_target -- --ignored --nocapture
```

This test uses the installed remote Rust runner and mock Claude executable. It
verifies SSH reachability, source transfer across a separate filesystem,
system-prompt delivery, the final agent summary, token counts, and cost.

## Lifecycle

Stop the container while retaining its volumes:

```powershell
.\scripts\ssh-target.ps1 stop
```

Remove the container and network while retaining its volumes:

```powershell
.\scripts\ssh-target.ps1 down
```

Re-running `up` reuses the remote root, origin, and host key. Remove all
fixture Docker volumes explicitly with:

```powershell
.\scripts\ssh-target.ps1 destroy
```

`destroy` deletes the fixture's Docker volumes and therefore its remote files
and bare origin. Generated client keys remain in `.docker-ssh-target` so an
accidental volume reset does not silently change the client identity.

Reset only the bare Git origin back to its pristine single-commit seed state,
without touching the remote root or the pinned host key -- useful between
destructive integration-test suites that push/rewrite branches on it, when a
full `destroy` (and the re-`up`/re-pin it forces) would be overkill:

```powershell
.\scripts\ssh-target.ps1 reset-origin
```

The bash counterpart accepts the same lifecycle actions:

```bash
scripts/ssh-target.sh up
scripts/ssh-target.sh down
scripts/ssh-target.sh destroy
scripts/ssh-target.sh reset-origin
```

## Container-backed machine fixture

The fixture above makes the *remote host* a container. To exercise the SSH
provider's [container-backed machines](machine-providers.md#container-backed-machines-docker-on-the-remote-host)
— where the work itself runs in a Docker container *on* the remote host — the
host needs its own Docker engine. `scripts/ssh-docker-target.sh` builds one
(docker-in-docker plus sshd, `docker/ssh-docker-target/`), builds the work image
(`docker/remote-agent/`: runner, git, tmux, mock `claude`), and loads it into the
host's inner engine, so the whole daemon → ssh → `docker exec` chain is real on
one machine:

```bash
scripts/ssh-docker-target.sh up      # also: stop | down | destroy | status | config
```

```powershell
$env:RALPHUS_SSH_DOCKER_CONTAINER_TEST = '1'
$env:RALPHUS_SSH_CONFIG_FILE = (Resolve-Path .docker-ssh-docker-target/ssh_config)
cargo build -p ralphus-ssh-provider
cargo nextest run -p ralphus-ssh-provider --test docker_container_target --run-ignored all
cargo nextest run -p ralphus-daemon --test provider_conformance --run-ignored all
```

`docker_container_target` covers container create/reuse/restart/recreate and
image-mismatch refusal, work provably running in the container (Debian, uid
10001, against an Alpine uid 10002 host), the bind-mounted remote root, the
legacy synchronous path, the durable async job lifecycle, descendant cancel, a
container restart reading as `lost`, binary-safe file operations and
`materialize`, and the terminal pty. The `provider_conformance` test drives the
same provider through the daemon's own `ProviderRunner` client. Run these with
nextest: each test is its own process, which the provider's process-wide
container setting needs. Fixture state lives in `.docker-ssh-docker-target/` (git-ignored),
port 2223. The host's inner Docker engine is privileged (docker-in-docker); this
fixture is for local testing only.

### A squad and a review through a real daemon

The tests above stop at the provider. To run a squad and a stacked review
through an actual (isolated) daemon on the container-backed machine:

1. Start a throwaway daemon the way `ralphus initialize` does: its own `--port`
   and `--db`, a private `HOME`/`USERPROFILE`, `RALPHUS_CONFIG_HOME`, and
   `PSMUX_DATA_DIR`, so nothing is shared with a regular dev stack. **A second
   daemon's startup reaps every `ralphus_` tmux session on the machine**
   (`tmux::reap_orphaned_sessions_at_startup` is machine-wide), so do this only
   when no live agent sessions matter.
2. Give the daemon a target in `$RALPHUS_CONFIG_HOME/config.toml`:
   `[machine.targets.e2e]` with `machine = "ssh-docker:ralphus-docker-docker"` and
   `remote_root = "/home/ralphus/.ralphus/remote-work"`.
3. Register the provider with every flag as one `--arg=--flag=value` token (see
   [Container-backed machines](machine-providers.md#container-backed-machines-docker-on-the-remote-host)),
   e.g. `ralphus machine register --scheme ssh-docker --program <provider> "--arg=--ssh-config=<ssh_config>" --arg=--container-image=ralphus-remote-agent:test "--arg=--container-mount=/srv/ralphus-work:/home/ralphus/.ralphus/remote-work"`.
4. Make a bare origin *inside the container* under the remote root, mirror it
   locally, and register the project with the local clone as `--path` and the
   container-reachable `file:///home/ralphus/.ralphus/remote-work/<origin>.git`
   as `--url`.
5. Submit tasks with `machine = "ssh-docker:ralphus-docker-docker"` whose cells
   commit and push their own branch, plus a `[[review]]` with the same
   `machine`. Remote tasks must push before they complete.

This ran green end to end: two dependent tasks ran as uid 10001 in the
container, the review's stacked rebase ran there too and reached `in_review`
with the container's committer identity, a mock-agent cell reported its tokens,
and cancelling a running squad left no process of its command tree behind.

### A real agent, and the board's Live View

The steps above use the mock `claude`. To run the real Claude Code CLI in the
container and watch it in the board:

1. Build the image with the CLI and load it into the fixture host's engine:
   `docker build -f docker/remote-agent/Dockerfile.claude-code -t
   ralphus-remote-agent:claude-code .`, then `docker save
   ralphus-remote-agent:claude-code | docker exec -i
   ralphus-ssh-docker-target-target-1 docker load`.
2. Give the container a private config directory holding a *copy* of
   `.credentials.json` (owned by uid 10001, mode 0600, plus a `.claude.json`
   containing `{"hasCompletedOnboarding":true}`), and mount it with
   `--arg=--container-mount=/srv/ralphus-claude:/home/ralphus/.claude` and
   `--arg=--container-run-arg=-e=CLAUDE_CONFIG_DIR=/home/ralphus/.claude`, with
   `--container-image` set to the new image. Delete the copy afterwards. See
   [Real agent CLIs and credentials](machine-providers.md#container-backed-machines-docker-on-the-remote-host).
3. Submit a task whose cell uses `agent = "claude-code"` and a `model`. Start
   `ralphus-librarian serve` against the same isolated daemon
   (`RALPHUS_DAEMON_URL`, the same private `HOME`) and open the cell's **Show Live
   View**: the streamed agent output advances while the cell runs and becomes the
   read-only historical record when it ends.
4. After the cell finishes, **Open Agent** (`POST .../terminal-ticket`, then the
   relay WebSocket) resumes the same session in a pty in the container.

### Running the same tests against a real second machine

The live tests address the machine only through the ssh alias
`ralphus-docker-docker`, so a real host needs no code change — give that alias
its own ssh config and run the suite against it:

1. On the host (Linux, sshd, Docker, the ssh user able to run `docker`), create
   the directory the tests bind-mount as the remote root, owned by the work
   image's uid: `sudo install -d -o 10001 -g 10001 -m 0750 /srv/ralphus-work`.
2. Put the work image there: build it where you like with
   `docker build -f docker/remote-agent/Dockerfile -t ralphus-remote-agent:test .`
   and `docker save ralphus-remote-agent:test | ssh <host> docker load`.
3. Write an ssh config containing a `Host ralphus-docker-docker` entry (real
   `HostName`/`User`/`IdentityFile`, and a `UserKnownHostsFile` pinned with
   `ssh-keyscan`; the provider is always `BatchMode` + `StrictHostKeyChecking`).
4. Point `RALPHUS_SSH_CONFIG_FILE` at it and run the two `nextest` commands
   above. Latency, a flaky link, and a hardened Docker setup (locked-down
   `docker` group, SELinux, rootless) are what this adds over the local fixture.

## What this fixture does not prove

The Linux target covers platform-neutral provider behavior. It does not prove:

- PowerShell command construction or Windows path handling;
- Windows process-tree cancellation;
- ConPTY terminal attachment;
- Git Credential Manager behavior in a Windows SSH login;
- Claude, Codex, or Pi authentication on a real Windows account.

Those require the live Windows-to-Windows acceptance pass in
`REMOTE_IMPROVEMENTS.local.md`.
