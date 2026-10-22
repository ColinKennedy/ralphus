# `ralphus-outpost`: machines without SSH

`ralphus-outpost` (`outpost/`) serves one machine's
[Machine API](machine-provider-service.md#machine-api) for `ralphus-fleet`
without anything listening on the machine. You copy the one executable to the
machine and run it; it connects **out** to the fleet over HTTP(S), waits for
requests, runs them on the machine and posts the results back. Use it for a
machine you cannot (or would rather not) reach over SSH: a Windows desktop, a
machine behind NAT, a container with no `sshd`.

The daemon cannot tell an outpost machine from an SSH one: it asks
`ralphus-fleet` for a machine, gets an allocation, and calls the allocation's
endpoint. Only the fleet knows that this machine's requests go through an
outpost.

## Setting one up

1. Make a pairing secret -- long and random, at least 32 characters:

   ```bash
   openssl rand -hex 32
   # PowerShell: -join ((1..32) | % { '{0:x2}' -f (Get-Random -Max 256) })
   ```

2. Give the fleet the machine, naming the environment variable that will
   hold the secret **in the fleet's environment**:

   ```bash
   ralphus-fleet add --outpost --name winbox --token-env WINBOX_OUTPOST_TOKEN \
     --remote-root C:/ralphus/fleet --label windows
   ```

   ```toml
   # ralphus-fleet.toml
   [[machine]]
   name              = "winbox"
   transport         = "outpost"
   outpost_token_env = "WINBOX_OUTPOST_TOKEN"
   remote_root       = "C:/ralphus/fleet"
   labels            = ["windows"]
   ```

   Start (or restart) `ralphus-fleet serve` with `WINBOX_OUTPOST_TOKEN` set.
   The fleet must listen where the machine can reach it (`listen =
   "0.0.0.0:7900"`, or behind a proxy).

3. On the machine, with the same secret in an environment variable:

   ```powershell
   $env:RALPHUS_OUTPOST_TOKEN = '<the secret>'
   ralphus-outpost connect http://fleet-host:7900 --name winbox `
     --token-env RALPHUS_OUTPOST_TOKEN --remote-root C:/ralphus/fleet
   ```

   It logs `connected to ... as "winbox"`, then one line per request (verb
   and id only). It reconnects by itself when the fleet restarts or the
   network drops. Stop it with Ctrl+C; jobs it started keep running and are
   picked up again when it reconnects.

`--remote-root` must match the fleet's `remote_root` for the machine: the
outpost refuses requests for any other root, so the machine's owner -- not
the fleet -- decides where ralphus may write.

| `[[machine]]` key | outpost machines |
|---|---|
| `name` | required: the `--name` the outpost connects with |
| `transport` | `"outpost"` |
| `outpost_token_env` | required: the variable in the fleet's environment holding the pairing secret |
| `remote_root` | required: must equal the outpost's `--remote-root` |
| `os`, `arch` | optional: reported by the outpost when it connects |
| `labels`, `max_allocations`, `agents` | as for SSH machines |
| `host`, `user`, `[machine.runner]` | not used (a runner table is an error) |

## What runs where

Every verb runs inside the outpost process through the machine-side verbs in
`ralphus-runner` (`runner/src/machine/`) -- the same code `ralphus-ssh-provider`
runs on a Windows machine over SSH -- with no shell in between: git is run as
a program, files are read and written directly, an authored command line goes
to `cmd` on Windows and `sh` elsewhere (or the shell the request names).

A cell (`exec`) runs as a durable job under `<remote_root>/jobs/`: the outpost
starts a detached supervisor (`ralphus-outpost runner machine supervise`),
which runs the cell with `ralphus-outpost runner send`. The outpost carries
`ralphus-runner`, so nothing else is copied to the machine and the runner is
always the outpost's own version. A job outlives the outpost; `status`,
`stream` and `cancel` find it by its handle after a reconnect. A supervisor
that stops beating its heartbeat (every 2 s) for 20 s is reported lost.

Agents use the machine's own installation and login: the outpost does not
install or log in an agent. Give an agent's path in the fleet's
`[machine.agents]` if it is not on the outpost's `PATH`. A command cell that
says `shell = "sh"` needs an `sh` on the machine's `PATH` (Git for Windows'
`usr\bin` provides one).

## Trust model

- **Who can make the machine do things:** whoever holds the pairing secret --
  in practice, the `ralphus-fleet` it was given to. Requests include `run`
  (any git command, or any authored shell command line in a workspace) and
  `exec` (any cell), so the secret grants code execution on the machine as the
  user running the outpost. Run the outpost as a user with access to nothing
  but what ralphus should touch.
- **Both sides authenticate.** The outpost will not take a request from a
  service that cannot prove it holds the secret (a spoofed URL or DNS name
  gets nothing done), and the fleet will not hand work to an outpost that
  cannot prove it holds it. The proofs are HMAC-SHA256 over both sides' random
  nonces; the secret itself never crosses the network.
- **Everything after the handshake is sealed** with ChaCha20-Poly1305 under a
  key derived for that session (HKDF-SHA256), with the direction and session
  bound in. Requests carry credentials (a job's `execution_environment`, a
  forge token for `git push`), so a passive observer of plain HTTP sees only
  ciphertext, and a tampered, reflected or replayed message is refused. HTTPS
  (a TLS proxy in front of the fleet) adds nothing secret-wise but is fine to
  use: `connect https://...` works.
- **What it does not protect against:** an attacker who learns the secret
  (it is equivalent to the machine), a compromised fleet host, or someone who
  controls the machine itself (who can read any running process's memory,
  credentials included). Traffic analysis (timing, sizes) is not hidden.
- **Where the secret lives:** in the fleet's and the outpost's environments
  and memory only. Neither writes it to disk, a log, or a process's command
  line; `ralphus-outpost` refuses a `--token` argument outright. A short
  secret is refused by both sides, since a captured handshake lets an
  eavesdropper test guesses offline.
- **Credentials reaching jobs** follow the rule every machine follows: in
  the job's process environment only, never in the job directory
  (`docs/machine-provider-service.md#credentials`).

## The wire protocol

All on the fleet's own listener, without the service token:

| Call | Body | Answer |
|---|---|---|
| `POST /v1/outpost/hello` | `{"protocol": "ralphus-outpost-v1", "name", "nonce"}` | `{"session", "nonce", "proof"}`, or `401` |
| `POST /v1/outpost/poll` | `{"session", "box": seal({"seq", "info"?})}` | after up to 20 s, `{"box": seal({"seq", "requests": [{"id", "verb", "body", "context"}]})}` |
| `POST /v1/outpost/reply` | `{"session", "box": seal({"id", "reply", "events"})}` | `{}` |

`seal` is base64 of a random 96-bit nonce and the ChaCha20-Poly1305
ciphertext, with associated data `ralphus-outpost-v1|<direction>|<session>`.
The fleet accepts a session only when the outpost's first sealed poll opens;
each poll's `seq` must grow and is repeated in the answer, so an old answer
cannot be replayed into a newer poll; each request id runs at most once per
session. A new session replaces the old one, failing that one's unanswered
requests. `GET /v1/outposts` (with the service token) lists the outpost
machines and whether each is connected.

## Same-host outposts and remote checks

An outpost can run on the daemon's own host (a test with its own
`remote_root`, say). That proves the request travelled the Machine API path --
fleet, sealed channel, outpost, machine-side verbs -- and that the daemon did
no remote work itself (machine strict mode still fails any local git or
command the daemon tries on the machine's behalf), but it does **not** prove
the work happened on another computer. [`scripts/fleet_e2e.py`](fleet-e2e.md)
reports which machines were outposts and leaves remoteness to how you placed
them.
