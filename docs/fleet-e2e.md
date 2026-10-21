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
| outpost machines connected | only when a machine has `transport = "outpost"` |
| live actions and exhibits work on the review's machine | only with `--live-actions`; see [below](#live-actions-and-exhibits---live-actions) |

No model is called: every cell and proof is a raw command (`sh` on a Linux or
macOS machine, Windows PowerShell on a machine whose `os` is `windows`), so a
run costs only machine time. It never touches a running dev stack; everything lives under
`--state-root` (a fresh directory each run).

## What you need

- Built binaries in one directory (default `target/debug`): `ralphus-daemon`,
  `ralphus-runner`, `ralphus-fleet`, `ralphus-ssh-provider`
  (`cargo build -p ralphus-daemon -p ralphus-runner -p ralphus-fleet -p ralphus-ssh-provider`).
- Two machines reachable over SSH, described in a `ralphus-fleet.toml` (see
  [Running `ralphus-fleet`](machine-provider-service.md#running-ralphus-fleet)),
  each with a label the harness can ask for, a `remote_root`, a runner
  (`[machine.runner]`) and `git`, `sh`, `grep`, `uname`, `hostname` (a
  Windows machine needs `git` and Windows PowerShell instead). They can be the
  same physical host (a WSL distribution and a container on it, say).
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

## Outpost machines

A machine with `transport = "outpost"` is not started by the harness: run
`ralphus-outpost connect` on it yourself ([`outpost.md`](outpost.md)), with its
pairing secret in the outpost's environment and the variable named by
`outpost_token_env` set in the harness's environment (the fleet inherits it).
Because outposts connect to the fleet, give the fleet a fixed address with
`--fleet-listen` (`0.0.0.0:7931` to let a WSL distribution or a container
reach it; the daemon still uses loopback). The harness waits up to
`--outpost-timeout` seconds for every outpost to connect, and they reconnect
by themselves when the harness restarts the fleet. When the machines cannot
reach this host at all (an inbound firewall), run `ralphus-fleet serve`
yourself somewhere they can, with its service token in
`$RALPHUS_E2E_FLEET_TOKEN`, and pass `--fleet-url` (the harness then neither
starts nor restarts it).

An outpost running on the daemon's own host (with its own `remote_root`) is
useful for testing the transport, but it does not prove remote execution:
strict mode still shows the daemon did no machine work itself, while the work
ran on the same computer through the outpost.

## Live actions and exhibits (`--live-actions`)

`--live-actions` (needs `--pr`) also proves that a review's
[live actions](daemon-api.md#live-actions) and [exhibits](exhibits.md) happen
on the review's machine (the `--label-a` one), still without a model. The
harness registers a `raw` agent profile, `fleet-e2e-agent`, whose executable is
a stand-in agent that a command cell in task A installs under that machine's
`remote_root` (`fleet-e2e-agent-<run>/agent`, a `sh` script; on Windows
`agent.exe`, compiled on the machine by Windows PowerShell's `Add-Type`, so
`remote_root` must be absolute). A `raw` agent gets the prompt as its last
argument and its stdout is the reply. A prompt cell in task A runs it and gets
back a `RALPHUS_LIVE_ACTION:` handoff asking for a screenshot and a GIF, and the
review (`agent = "fleet-e2e-agent"`, `live_actions = true`) runs it for its live
runs: it writes the machine's hostname and working directory into a new file and
commits it, saves a small PNG and GIF into the run's exhibit directory and
reports them with `RALPHUS_EXHIBIT:` lines, then a `RALPHUS_LIVE_RESULT:` line.
The PR writer cannot resolve a database agent profile, so the PR text is
ralphus's deterministic fallback.

| Check | How |
|---|---|
| the cell's live handoff became a live action | `GET /api/guardians/{id}/live-actions`: one action with the handoff's label, sourced from this squad |
| the automatic and the manual live run ran on the review's machine | the automatic run, then one queued with `POST .../live-actions/run` (what `ralphus review action run` does): each `finished`, its `cwd` and the cwd the agent reported both under machine A's `remote_root`, plus the strict-mode check above |
| each run's commits were carried onto the top branch | the run's `integration` is `integrated` |
| the live runs' reports were posted into the review | both reports in the top branch's messages |
| the exhibits were uploaded from the machine | `GET /api/guardians/{id}/exhibits`: both slots have a URL and no file kept on the daemon host |
| the live results and exhibits are in its PR | GitHub API: task A's PR body has the `## Live-Test Results` and `## Exhibits` sections, the action's label and every slot's URL (waits up to `--live-timeout`) |
| another branch's PR shows none of them | GitHub API: task B's PR body has neither the label nor the action's exhibit blocks |
| the uploaded files are reachable | each URL fetched with the token (dropped when GitHub redirects to its storage host): HTTP 200 and a PNG/GIF |
| the live commits are on the top branch's PR | GitHub API: every file the runs committed is on the top branch's PR head |
| the live runs ran on the same host as task A's cell | the hostname in those files matches the one task A's cell committed |
| the forge token is not on the machine's disk | an audit squad on machine A gets the token as a registered secret and searches `remote_root` and ralphus's cache there (`~/.cache/ralphus`, `%LOCALAPPDATA%\ralphus`) |

Without `--live-actions`, but with `--forge-token-env`, the run also checks
that the forge token is in no file under `--state-root` (logs, configs, the
fleet's state) except the daemon's database, where a stored token belongs.

The PR-body checks read GitHub's REST API, so a repository elsewhere records
them as not checked. The runs add `fleet-e2e-live-<run>-*.txt` commits to the
review's top branch, and the stand-in agent stays under `remote_root`; delete
them with the branches if you do not want them to accumulate.

## Not covered

The base branch moving under an open review is not part of this run yet, and
`--live-actions` checks PR bodies on GitHub only.
