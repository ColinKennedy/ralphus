# Machine provider services (provider contract v2)

> **Status:** the daemon side is implemented -- configuration, request
> validation, allocation, the Machine API transport, re-attach after restart,
> release and idle hints, and the read-only allocations view (board, CLI,
> MCP). `ralphus-fleet` reaches Linux and Windows machines over SSH, and any
> machine through `ralphus-outpost` ([`outpost.md`](outpost.md)). Program providers
> ([`machine-providers.md`](machine-providers.md), contract v1) keep working
> unchanged; this page describes the service form that sits alongside them.

A **provider service** is a long-running process that hands ralphus machines.
Ralphus never adds, enrolls, connects to or chooses a machine itself: it asks a
provider for one that satisfies a request, and either gets an **allocation** or
an error that says why not. How the provider finds machines, gets code onto
them and picks among them is entirely its own business. `ralphus-fleet` is the
built-in provider service and the reference implementation of this contract.

## Configuring a provider service

Provider services are declared in the global `config.toml` -- an
administrator's file, never a task file (the reasoning in
[Why this is not declarable in TOML](machine-providers.md#why-this-is-not-declarable-in-toml)
applies unchanged):

```toml
[machine.providers.fleet]
url       = "http://127.0.0.1:7900"   # where the service listens
token_env = "RALPHUS_FLEET_TOKEN"     # env var holding its bearer token
```

| Key | Required | Meaning |
|---|---|---|
| `url` | yes | Base URL of the service. Must parse as `http://` or `https://`. |
| `token_env` | yes | Name of the environment variable the daemon reads the service's bearer token from. Must be a valid variable name. The token itself is never written to config. |

Validation is **strict on shape, soft on availability**: an unknown key, a
`url` that is not http(s), or an invalid `token_env` name is a configuration
error. An unset token variable or an unreachable service at startup does not
stop the daemon -- that provider is reported unavailable (with remediation) and
only work requesting it fails.

Providers are tried in **declaration order** wherever ralphus walks several of
them (see below).

### `local`

`local` is a built-in provider that is always present and never declared. It
hands out the daemon's own host. Declaring `[machine.providers.local]` is a
configuration error, as is registering a program provider under that name.

## Requesting a machine

A task, cell, proof step or review asks for a machine with `machine`, which
inherits as described in [The `machine` value](machine-providers.md#the-machine-value).
Three forms:

```toml
machine = "local"                                    # the daemon's host
machine = "fleet"                                    # any machine provider "fleet" picks
machine = { provider = "fleet", os = "windows" }
machine = { provider = "fleet", os = "linux", arch = "x86_64", labels = ["gpu"] }
machine = { provider = ["fleet", "local"], os = "linux" }      # fleet if it can, else here
machine = { provider = "<<ralphus:auto-provider>>", os = "linux" }  # any configured provider
```

The legacy `<scheme>:<uri>` string still addresses a program provider.

| Key | Values | Validated |
|---|---|---|
| `provider` | a name, a list of names (tried in order), or `<<ralphus:auto-provider>>` | offline: names match `[A-Za-z0-9_-]{2,}`, a list is non-empty without duplicates, the placeholder is not inside a list; at submit: every named provider is configured (or `local`), and the placeholder needs at least one configured provider |
| `os` | `windows`, `linux`, `macos` | closed set |
| `arch` | `x86_64`, `aarch64` | closed set |
| `labels` | strings of `[A-Za-z0-9_.-]` | non-empty; otherwise opaque to ralphus -- their meaning is the provider's |

Unknown keys are errors, reported with their line number.

### Walking providers

For a list, or the placeholder, ralphus asks each provider in turn to allocate
a machine for the rest of the request; the first that grants one wins. The
provider still decides -- "matches" means "granted". An unavailable provider is
skipped. When none grants it, the submission's error lists every provider tried
with its refusal reason and remediation.

- `<<ralphus:auto-provider>>` walks the configured providers in declaration
  order and does **not** include `local`; list `local` explicitly to allow
  falling back to the daemon's host.
- `local` grants a request when the daemon host's os/arch satisfy it and no
  labels are asked for. Because the host is known at submit, an explicit
  `provider = "local"` that cannot match is rejected at submit.

## Allocations

An **allocation** is one machine handed to ralphus for a scope of work:

```json
{
  "allocation_id": "alloc-7f3a",
  "endpoint": "http://127.0.0.1:7900/v1/m/alloc-7f3a",
  "token": "...",
  "token_expires_at_ms": 1760000000000,
  "os": "linux",
  "arch": "x86_64",
  "labels": ["gpu"],
  "description": "mybox.local/wsl-ubuntu"
}
```

- **Scope.** One allocation per *(squad, normalized machine request)*. Every
  task, cell, proof and review in the squad with an equal request shares it,
  which keeps the existing affinity rule ("the same `machine` value means the
  same machine": a task's cells together, a review on its cells' machine). A
  walk runs once per scope and its result is pinned.
- **When.** A squad's requests are granted before any worktree is made for it:
  at submit when its cells feed a review, otherwise when the scheduler picks
  it up. A finished review is never given a machine. A review's own request (`[[review]]
  machine` or the project default) the first time the review is used, reusing
  an allocation one of its squads holds for the same request so the review
  merges where its cells ran. A request no provider grants fails the squad
  with a mailbox error listing every provider tried and why.
- **Persistence.** Allocations are stored, without their tokens. After a
  daemon restart each live allocation is re-read (`GET`) and gets a new token
  (`renew`) on first use; one the provider no longer knows is marked lost and
  reported in the mailbox with how to get a new machine (restarting the squad
  or reopening the review asks again).
- **Tokens** are short-lived and renewed by the daemon before they expire.
- **Release.** When the squad is terminal and every review fed by it is merged
  or deleted. An open review holds its allocation (its merge worktree and any
  in-progress rebase live there) and marks it **idle** while it waits on a
  human, so the provider can act on that (power the machine down, say) as long
  as it resumes on the next call.
- **Background calls from an open review.** Each git command of a review on a
  machine is its own Machine API call. An open review's background
  maintenance (base-shift and manual-push checks, PR branch sync) and its
  base-branch fetch run every **30 s** for a review on a machine, against 5 s
  and 10 s for a review on the daemon's host, so a review merely waiting on a
  human costs its machine roughly two dozen short commands a minute. A
  collecting review's change summary is recomputed at most every 30 s while
  its page is being polled.

## Provider service API

All requests carry `Authorization: Bearer <token from token_env>`. Bodies are
JSON. Every reply carries `"protocol_version": 2`.

| Call | Request | Reply |
|---|---|---|
| `POST /v1/allocations` | `{"requirements": {"os"?, "arch"?, "labels"?: []}, "scope": {"squad_id" or "review_id", "key"}}` | an allocation (above), or an error |
| `GET /v1/allocations/{id}` | -- | the allocation without a token, or `404` |
| `POST /v1/allocations/{id}/renew` | -- | the allocation with a new token |
| `POST /v1/allocations/{id}/idle` | `{"idle": bool}` | `{}` -- a hint |
| `DELETE /v1/allocations/{id}` | -- | `{}` |
| `GET /v1/health` | -- | `{"version": "..."}` |

## Machine API

The daemon calls the allocation's `endpoint` with `Authorization: Bearer
<allocation token>`. Whether the endpoint is the provider itself (proxying to
the machine) or an agent on the machine is the provider's choice; ralphus
cannot tell the difference.

Each operation is `POST <endpoint>/<op>` with the same JSON request the v1 verb
of that name takes on stdin, and the same reply fields:

| Op | Request (v1 struct) | Notes |
|---|---|---|
| `provision` | `ProvisionRequest` | returns `workspace`, the path in the machine's own form |
| `exec` | `RunnerSpec` | `result` or `handle`; secrets travel only in this body |
| `status` / `stream` / `cancel` / `job-cleanup` | `{"handle", "since"?}` | as v1 |
| `run` | `RunRequest` | one VCS command, or one authored shell command when `program` is empty |
| `read-file` / `write-file` / `remove-path` | `FileRequest` | as v1 |
| `materialize` | `MaterializeRequest` | streams the file back in the reply body |
| `capabilities` / `ping` | `{}` | `capabilities.os` decides path flavour and shell syntax |
| `cleanup` / `retire` | `CleanupRequest` / `RetireRequest` | as v1 |

**Events.** A program provider surfaces the runner's `RALPHUS_EVENT:` lines on
stderr. Over HTTP, `exec`, `status` and `stream` replies carry them instead as
`"events": ["<json>", ...]`, oldest first, which the daemon forwards exactly as
it forwards stderr markers -- including the `llm-invoke` usage events the live
cost cap depends on.

## Errors

A refusal is `{"ok": false, "protocol_version": 2, "error": "...",
"reason_code": "...", "remediation": "..."}` with an HTTP 4xx/5xx status.

| `reason_code` | Meaning |
|---|---|
| `no_matching_machine` | nothing the provider manages satisfies the requirements |
| `capacity` | matching machines exist but none is free now |
| `unreachable` | the machine or provider cannot be reached |
| `unauthorized` | the token was refused |
| `unknown_allocation` | the allocation id is not (or no longer) known |
| `unsupported` | the provider does not implement the operation |

`remediation` is shown to the user verbatim, so it should say what to do.

## Running `ralphus-fleet`

`ralphus-fleet` (`fleet/`) is the built-in provider service. Each machine in
its configuration names a **transport**:

- `ssh` (the default): the fleet runs `ralphus-ssh-provider` (found next to
  its executable, or set `ssh_provider`) against the machine, so it needs only
  an SSH login -- a Linux box, a WSL distribution with `sshd`, a container
  running OpenSSH (`scripts/ssh-target.sh`), or a Windows machine with the
  built-in OpenSSH Server (see [Windows over SSH](#windows-over-ssh)).
- `outpost`: the machine runs `ralphus-outpost connect`, which connects out to
  the fleet and runs its requests; nothing listens on the machine. See
  [`outpost.md`](outpost.md).

```bash
ralphus-fleet add wslbox --user me --remote-root /home/me/ralphus-fleet --label gpu
export RALPHUS_FLEET_TOKEN=$(openssl rand -hex 32)   # the daemon gets the same value
ralphus-fleet serve                                  # reads ./ralphus-fleet.toml
ralphus-fleet list                                   # machines and their allocations
```

```toml
# ralphus-fleet.toml
listen          = "127.0.0.1:7900"
token_env       = "RALPHUS_FLEET_TOKEN"
ssh_config_file = "C:/ralphus/ssh_config"     # optional `ssh -F`

[[machine]]
host        = "wslbox"                        # hostname, user@host, or ssh-config alias
user        = "me"
os          = "linux"                         # optional: asked of the machine when omitted
labels      = ["gpu"]
remote_root = "/home/me/ralphus-fleet"
max_allocations = 4                           # optional
[machine.runner]
mode      = "upload"                          # or "installed" (default)
artifacts = { "x86_64-unknown-linux-musl" = "C:/ralphus/linux/ralphus-runner" }
[machine.agents]
codex = "/home/me/.local/bin/codex"           # agent executables on that machine

[[machine]]
name              = "winbox"                  # reached through ralphus-outpost
transport         = "outpost"
outpost_token_env = "WINBOX_OUTPOST_TOKEN"
remote_root       = "C:/ralphus/fleet"
labels            = ["windows"]
```

### Configuration reference

Top level:

| Key | Default | Meaning |
|---|---|---|
| `listen` | `127.0.0.1:7900` | `host:port` to listen on (outposts must reach it) |
| `public_url` | `http://<listen>` (loopback for `0.0.0.0`) | the URL the daemon reaches the service at; set it when the daemon is on another host |
| `token_env` | required | variable holding the service token the daemon presents |
| `ssh_provider` | next to `ralphus-fleet`, then `PATH` | the `ralphus-ssh-provider` program |
| `ssh_config_file` | OpenSSH default | `ssh -F` file for every SSH machine |
| `state_file` | `<config>.state.json` | where allocations (never tokens) are kept |

Each `[[machine]]`:

| Key | Transport | Meaning |
|---|---|---|
| `name` | both (required for `outpost`) | how allocations name it; `host` when omitted |
| `transport` | -- | `ssh` (default) or `outpost` |
| `host`, `user` | `ssh` | SSH destination: hostname, `user@host`, or a `~/.ssh/config` alias |
| `outpost_token_env` | `outpost` | variable in the fleet's environment holding the machine's pairing secret |
| `os`, `arch` | both | `linux`/`windows`/`macos`, `x86_64`/`aarch64`; probed (SSH) or reported (outpost) when omitted |
| `labels` | both | opaque labels a request can ask for |
| `remote_root` | both | durable storage root on the machine |
| `max_allocations` | both | at most this many allocations at once |
| `[machine.runner]` | `ssh` | `mode` (`installed`/`upload`), `command`, `artifacts` (target triple to runner binary on the fleet host) |
| `[machine.agents]` | both | agent name to executable path on the machine |

Validation is strict: unknown keys, an `ssh` machine without `host`, an
`outpost` machine without `name` or `outpost_token_env` (or with a
`[machine.runner]`), and a Windows SSH machine whose `remote_root` contains
anything but letters, digits and `. _ - \ /` are errors.

### Windows over SSH

A Windows machine with the built-in OpenSSH Server works over the `ssh`
transport with every verb the Linux path supports. Declare `os = "windows"`
(or leave it out: a machine without `uname` that answers `echo %OS%` with
`Windows_NT` is probed as Windows). The fleet passes the machine's os and
arch to `ralphus-ssh-provider`, which then runs each verb on the machine as
`<runner> machine <verb> --remote-root <root>` with the request on the
session's stdin, instead of POSIX shell scripts:

- the session's default shell must be `cmd.exe` (OpenSSH's default);
- `remote_root` (and an `installed` runner's path) must be a drive path of
  letters, digits and `. _ - \ /` -- it goes on the command line unquoted;
- in `upload` mode the runner (`ralphus-runner.exe`, artifact key
  `x86_64-pc-windows-msvc` or `aarch64-pc-windows-msvc`) is copied on the
  first verb that finds it missing, to
  `<remote_root>\runners\<triple>\<sha256>\ralphus-runner.exe`: `scp` (the
  SFTP subsystem Windows' OpenSSH Server ships with) copies it to a
  temporary name, and a PowerShell script passed as `-EncodedCommand` checks
  its SHA-256 before moving it into place (Windows PowerShell 5.1 does not
  read more than a few bytes of binary stdin under sshd, so the runner never
  travels on stdin);
- `exec` starts a detached job supervisor outside the session's job object;
  if Windows refuses that, the job runs in the foreground of the `exec` call
  and its result comes back directly (bounded by the daemon's HTTP timeout);
- credentials travel on the session's stdin and reach only the job's process
  environment; agents use the machine's own installation and login.

- A request is granted on the least-loaded machine that matches its
  `os`/`arch`/`labels` and has room; a refusal lists why each machine did not
  fit.
- A repeated request for the same scope gets the same allocation.
- Allocations are kept in `<config>.state.json` (or `state_file`) so they
  survive a restart; their tokens are not, so after a restart the daemon's
  first call gets `401` and it renews.
- The service fills in the machine's `remote_root`, runner policy and agent
  paths; the daemon needs no `[machine.targets]` entry for it.
- `materialize` copies the selected files to the fleet host and returns them
  in the reply, so the daemon may run elsewhere.
- `GET /v1/outposts` (service token) lists outpost machines and whether each
  is connected.

To check a set of machines end to end (strict mode, two machines, a review,
secrets, restarts, release), run [`scripts/fleet_e2e.py`](fleet-e2e.md). To
write a provider of your own, see [`writing-a-provider.md`](writing-a-provider.md).

## Program providers keep working

A program provider (v1) is adapted rather than replaced: its allocation is its
`<scheme>:<uri>` machine value (no allocate/release round trip), and each
Machine API operation is the verb of the same name. Everything above the
adapter -- strict mode, the review and PR paths, the scheduler -- sees one
interface.

## Credentials

The daemon owns secrets; a provider never stores them. They reach a job only in
the `exec` request body and live only in that job's process environment -- never
in a file on the machine, a command line, a log or a transcript. A provider that
proxies sees them in transit; that is inherent, since whoever controls a
machine can read a running process's memory.

### Git pushes from a machine

The daemon pushes a review's branches and a remote cell's published branch by
running `git push` on the machine. When the project's clone URL is HTTPS, that
one git process (and any `fetch`, `pull` or `ls-remote`) gets the forge token
of the review's owner (or the squad's submitter, or the daemon's default user)
in its environment, with an inline `credential.helper` that answers only for
the clone URL's host. Nothing is written to the machine's git config or disk,
and the value never appears in a process listing. Over SSH the machine's own
keys are used, as before. Git run by an agent inside a cell is not given a
credential.

### Logging an agent in on a remote machine

Give the agent its credential through the environment, never a file copied to
the machine. For Claude Code, create a long-lived token on any machine where
you are logged in (`claude setup-token`) or use an API key, keep it in the
daemon's environment, and link it into an agent profile:

```bash
# daemon's environment (e.g. its service definition), not a task file:
RALPHUS_REMOTE_CLAUDE_TOKEN=<token from `claude setup-token`>

ralphus agent profile register --name claude-remote --backend claude-code \
  --env-link CLAUDE_CODE_OAUTH_TOKEN=RALPHUS_REMOTE_CLAUDE_TOKEN
# or, with an API key:  --env-link ANTHROPIC_API_KEY=RALPHUS_REMOTE_ANTHROPIC_KEY
```

Cells using `agent = "claude-remote"` then receive the variable in the `exec`
body on every run (re-read each time, so rotating it needs no re-save). The
SSH provider hands it to the runner through the launching session's
environment; it is never written to the machine's disk, and its value is
redacted in every API and board response.
