# Docker SSH target fixture

This fixture runs a Linux/OpenSSH machine target in Docker while the Ralphus
daemon remains on the host. It exercises the real SSH provider boundary and is
separate from [container mode](container-mode.md), where the daemon and runner
live together inside one container.

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
cargo test -p ralphus-ssh-provider `
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

## What this fixture does not prove

The Linux target covers platform-neutral provider behavior. It does not prove:

- PowerShell command construction or Windows path handling;
- Windows process-tree cancellation;
- ConPTY terminal attachment;
- Git Credential Manager behavior in a Windows SSH login;
- Claude, Codex, or Pi authentication on a real Windows account.

Those require the live Windows-to-Windows acceptance pass in
`REMOTE_IMPROVEMENTS.local.md`.
