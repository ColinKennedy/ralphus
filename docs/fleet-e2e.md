# Remote machines end to end (`scripts/fleet_e2e.py`)

`scripts/fleet_e2e.py` checks that ralphus really does its remote work on
remote machines. It starts an isolated daemon in
[machine strict mode](machine-providers.md#machine-strict-mode-development-only) and a
`ralphus-fleet` service in front of machines you describe, runs one squad whose
two tasks land on two different machines and feed a review that merges on the
first, and prints a pass/fail line for each check:

| Check | How |
|---|---|
| the project needs no clone on the daemon host | registered remote-only (unless `--local-clone`) |
| the squad finished `done` | squad state |
| each task ran on the machine its labels asked for | `GET /api/machine-allocations` |
| the review reached `in_review` on its machine | review status |
| no remote work ran on the daemon host | no `machine strict mode: refused` line in the daemon log |
| a registered secret reached the cell and is not on the machine's disk | the cell itself tests `$RALPHUS_E2E_SECRET` and greps the machine's `remote_root` for its value |
| the secret is not in the daemon log | log scan |
| the work survives a `ralphus-fleet` restart | the service is restarted while the squad runs (tokens are re-issued) |
| a request no machine can serve fails with the provider's reasons | a second squad asks for an os no machine has |
| the machines are handed back once the review is cancelled | allocations released within `--release-timeout` |
| the review's pull request opens from the machine | only with `--pr` |

No model is called: every cell and proof is a raw `sh` command, so a run costs
only machine time. It never touches a running dev stack; everything lives under
`--state-root` (a fresh directory each run).

## What you need

- Built binaries in one directory (default `target/debug`): `ralphus-daemon`,
  `ralphus-runner`, `ralphus-fleet`, `ralphus-ssh-provider`
  (`cargo build -p ralphus-daemon -p ralphus-runner -p ralphus-fleet -p ralphus-ssh-provider`).
- Two machines reachable over SSH, described in a `ralphus-fleet.toml` (see
  [Running `ralphus-fleet`](machine-provider-service.md#running-ralphus-fleet)),
  each with a label the harness can ask for, a `remote_root`, a runner
  (`[machine.runner]`) and `git`, `sh`, `grep`, `uname`, `hostname`. They can
  be the same physical host (a WSL distribution and a container on it, say).
  The harness replaces the file's `listen`, `token_env` and `state_file`.
- A test repository both machines can clone, with a `main` branch (or pass
  `--base`). It is registered as a remote-only project (no clone on the daemon
  host); `--local-clone` clones it here and registers that path instead.
- For an HTTPS repository, or with `--pr`: a forge token in an environment
  variable (`--forge-token-env`). Pushes from the machines then authenticate
  with it; over SSH the machines' own keys are used.
- Python 3.11+ (standard library only).

## Running it

```bash
cargo build -p ralphus-daemon -p ralphus-runner -p ralphus-fleet -p ralphus-ssh-provider
GH_TOKEN=$(gh auth token) python scripts/fleet_e2e.py \
  --fleet-config ~/fleet/ralphus-fleet.toml \
  --repo-url https://github.com/me/ralphus-e2e.git \
  --label-a wsl --label-b docker \
  --forge-token-env GH_TOKEN
```

Exit status 0 means every check passed. Logs, configs, the submitted squad and
both services' state stay under the printed `--state-root` for inspection;
`--keep` leaves the daemon and `ralphus-fleet` running to poke at afterwards.

The run commits `fleet-e2e-a-<run>.txt` and `fleet-e2e-b-<run>.txt` on two new
branches. Task B's branch is pushed to `refs/ralphus/cells/...` on the test
repository so the review on machine A can fetch it, and `--pr` opens a pull
request; delete those afterwards if you do not want them to accumulate.

## Not covered

The base branch moving under an open review, live actions and exhibits, and a
Windows machine are not part of this run yet.
