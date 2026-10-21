# `ralphus-outpost`: machines without SSH

`ralphus-outpost` (`outpost/`) serves one machine's
[Machine API](machine-provider-service.md#machine-api) for `ralphus-fleet`
without anything listening on the machine. It connects **out** to the fleet
over HTTP(S), waits for requests, runs them on the machine and posts the
results back. Use it for a machine you cannot (or would rather not) reach over
SSH: a Windows desktop, a machine behind NAT, a container with no `sshd`.

The daemon cannot tell an outpost machine from an SSH one: it asks
`ralphus-fleet` for a machine, gets an allocation, and calls the allocation's
endpoint. Only the fleet knows that this machine's requests go through an
outpost.

## Set up a machine once

You need `ralphus-fleet serve` running somewhere the machine can reach
(`listen = "0.0.0.0:7900"`, set `public_url` to the address machines use), and
the `ralphus-outpost` executable for the machine's platform.

1. **On the fleet host**, add the machine and get a pairing code:

   ```bash
   ralphus-fleet add --outpost --name winbox --remote-root C:/ralphus/fleet \
     --label windows --require-agent claude-code
   # restart `ralphus-fleet serve` so it serves the new machine, then:
   ralphus-fleet pair winbox
   ```

   `pair` prints a one-time code like `K7QM-2XRD-9FHT-B4WN` and the exact
   command to run on the machine. The code works once and expires after ten
   minutes; run `pair` again for a new one.

2. **On the machine**, as the user ralphus should run as, copy
   `ralphus-outpost` anywhere and run what `pair` printed:

   ```powershell
   # Windows (no administrator needed)
   .\ralphus-outpost.exe install --fleet http://fleet-host:7900 --code K7QM-2XRD-9FHT-B4WN
   ```

   ```bash
   # Linux
   ./ralphus-outpost install --fleet http://fleet-host:7900 --code K7QM-2XRD-9FHT-B4WN
   ```

   On Windows, run it at the machine's desktop (or over Remote Desktop): a
   session signed in over SSH with a key cannot use the Credential Manager,
   so `install` there pairs but cannot keep the credential. From SSH, start
   it in your logon session through a one-time Scheduled Task instead
   (`schtasks /create /tn ralphus-outpost-once /sc once /st 00:00 /it /tr
   "<the install command>"`, `schtasks /run /tn ralphus-outpost-once`, then
   `schtasks /delete /tn ralphus-outpost-once /f`); `install` says so when
   it hits this.

That is all. `install` pairs with the fleet, keeps the machine's credential in
the operating system's encrypted per-user store, copies itself into the
outpost's home directory, registers itself to start automatically, and starts.
From then on the machine connects by itself after every reboot, logon,
sleep/wake or network change, and keeps itself on the fleet's runner.

- `ralphus-outpost status` -- settings, whether the credential is present
  (never its value), the autostart, the last connection state and the log's
  last lines.
- `ralphus-fleet list` (on the fleet host) -- each machine's pairing, its
  readiness and what to fix.
- `ralphus-outpost uninstall` -- stop it, remove the autostart, delete the
  credential. `ralphus-fleet revoke winbox` forgets it on the fleet side
  (do this too if the machine is lost: its credential stops working at its
  next poll).

| Platform | `install` registers | `install --service` |
|---|---|---|
| Windows | a per-user Scheduled Task (`ralphus-outpost`) that starts at logon and, if the outpost stops or is killed, starts it again within a minute, run hidden through `conhost --headless` | a Scheduled Task that starts at **boot** as you, whether or not you are logged on. Run it from an elevated prompt; Windows asks for your password once and keeps it itself (ralphus never sees it) |
| Linux | a `systemd --user` unit (`Restart=always`); with no user systemd, an XDG autostart entry (starts with your desktop session) | the same unit plus `loginctl enable-linger`, so it starts at boot |
| macOS | a launchd agent (`com.ralphus.outpost`, `KeepAlive`) | not supported yet: a launchd daemon cannot read your Keychain |

`install --dry-run <dir>` writes the definition it would register into
`<dir>` and prints the commands it would run, pairing and changing nothing.

The outpost's home is `%LOCALAPPDATA%\ralphus\outpost` on Windows,
`~/.local/state/ralphus/outpost` on Linux (`$XDG_STATE_HOME`) and
`~/Library/Application Support/ralphus/outpost` on macOS
(`RALPHUS_OUTPOST_HOME` or `--home` overrides it). It holds `outpost.toml`
(fleet URL, machine name, `remote_root`, which store holds the credential --
no secret), the installed executable in `bin/`, downloaded runners in
`runners/`, `status.json`, and the log in `logs/outpost.log` (rotated at
5 MB, three old files kept; it records connections, request verbs and ids,
never a secret).

### When the machine cannot reach the fleet host

If inbound connections to the fleet host are blocked (a firewall, the fleet
on a laptop behind NAT) but the machine is reachable over SSH from the fleet
host, carry the outpost's connection over a reverse tunnel opened **from the
fleet host**:

```bash
# on the fleet host: the machine's localhost:7900 now reaches this fleet
ssh -N -R 7900:127.0.0.1:7900 me@winbox
```

and install with `--fleet http://127.0.0.1:7900` on the machine. Keep the
tunnel up with your usual tool (`autossh -M 0 -N -R ...`, a systemd unit, a
Scheduled Task). The outpost's channel is authenticated and sealed end to
end, so the tunnel adds reachability, not trust. (If the fleet host can reach
the machine over SSH anyway, the `ssh` transport may be simpler.)

## Readiness

Each time it connects, the outpost checks its machine and sends the fleet a
**readiness report**:

| Check | Fails / warns when | Fix it shows |
|---|---|---|
| `git` | git is not on `PATH` (fails) | install Git (for Windows) and restart the outpost |
| `agent:<name>` | a CLI agent (`claude-code`, `codex`, `pi` -- always checked) or one from `[machine.agents]` / `required_agents` cannot start, or its configured path does not exist (fails) | install it for the outpost's user, or set its path in `[machine.agents]` |
| `agent-login:<name>` | Claude Code or Codex is installed but not logged in (fails), or its status is unclear (warns) | Claude Code: run `claude`, then `/login`; Codex: `codex login` -- on the machine, as the outpost's user |
| `long-paths` | Windows `LongPathsEnabled` is not 1 (warns) | the `reg add` and `git config core.longpaths` commands |
| `disk` | under 5 GB free on `remote_root`'s volume (warns) | free space, or move `remote_root` |

The login check runs the agent's own status command (`claude auth status`,
`codex login status`) and keeps only "logged in or not": no account, email
or token reaches the report, the log or the fleet. `ralphus-fleet list` shows
each machine's agents (ready, not installed, NOT LOGGED IN) and the fixes.

The daemon tells the fleet which agents a squad's work needs on the machine
(`requirements.agents`, see
[the provider API](machine-provider-service.md#provider-service-api)), and
the machine's `required_agents` add a floor. A machine whose git check fails,
or that has one of those agents not installed or not logged in, is not
allocated: the request is refused before any work reaches a machine, with a
remediation in plain words -- "Claude Code on winbox isn't logged in — on
that machine run `claude`, then `/login`". A login status that could not be
told does not block; the cell's own preflight still runs.


## The runner

With no `[outpost_runners]` entry for its platform, an outpost runs jobs with
the `ralphus-runner` built into itself (`ralphus-outpost runner ...`), so
nothing else needs copying. With one:

```toml
[outpost_runners]
"windows-x86_64" = "C:/ralphus/win/ralphus-runner.exe"
"linux-x86_64"   = "/srv/ralphus/linux/ralphus-runner"
```

every poll answer carries that file's sha256. An outpost whose cached runner
differs downloads it over the sealed channel, checks the sha256, installs it
into `runners/<sha256>/` and points new jobs at it. A job already running
keeps the runner it started with; an old runner is deleted only once no
unfinished job uses it. Replacing the file on the fleet host rolls every
outpost of that platform forward at its next poll.

## Without pairing: a shared secret

For tests and harnesses, an outpost can also run in the foreground with a
shared pairing secret instead of a paired credential:

```bash
openssl rand -hex 32                                     # the secret
ralphus-fleet add --outpost --name box --token-env BOX_OUTPOST_TOKEN --remote-root /srv/r
# fleet: BOX_OUTPOST_TOKEN=<secret> ralphus-fleet serve
# machine:
RALPHUS_OUTPOST_TOKEN=<secret> ralphus-outpost connect http://fleet-host:7900 \
  --name box --token-env RALPHUS_OUTPOST_TOKEN --remote-root /srv/r
```

The secret is read from the environment only (`--token` is refused) and must
be at least 32 characters. A machine may have both a secret and a pairing.

`--remote-root` (or, when paired, the fleet's `remote_root` the outpost
learned when it paired) is the only root the outpost serves: it refuses
requests for any other, so the machine's owner sees and accepts where ralphus
may write. `install --remote-root <path>` makes `install` refuse to pair with
a fleet that names a different one.

| `[[machine]]` key | outpost machines |
|---|---|
| `name` | required: the machine's name (paired outposts learn it when they pair) |
| `transport` | `"outpost"` |
| `outpost_token_env` | optional: the variable in the fleet's environment holding a shared pairing secret |
| `remote_root` | required |
| `os`, `arch` | optional: reported by the outpost when it connects |
| `labels`, `max_allocations`, `agents`, `required_agents` | as for SSH machines |
| `host`, `user`, `[machine.runner]` | not used (a runner table is an error; see `[outpost_runners]`) |

### When the machine cannot reach the fleet: an SSH reverse tunnel

An outpost connects *to* the fleet, so the fleet host must accept that
connection. When an inbound firewall on the fleet host blocks it (a Windows
PC on a "Public" network, say) and you would rather not change firewall
rules on either side, but you can SSH from the fleet host to the machine,
carry the outpost's connection back over one SSH session instead. Nothing
then listens on the fleet host beyond loopback, and the only connection
crossing the network is your outgoing SSH.

1. Run the fleet on loopback (`listen = "127.0.0.1:7931"`), with the
   machine's pairing secret in its environment as above.
2. Copy `ralphus-outpost` to the machine (`scp`), for example to
   `C:\ralphus\bin\ralphus-outpost.exe`.
3. From the fleet host, open one long-lived session that forwards the
   machine's `127.0.0.1:7931` back to the fleet and runs the outpost in its
   foreground, connecting to that forwarded port:

   ```bash
   ssh -o ExitOnForwardFailure=yes -o ServerAliveInterval=15 \
       -R 7931:127.0.0.1:7931 me@winbox \
       powershell -NoProfile -NonInteractive -Command -
   ```

   and write one line to that session's stdin (from a program, not typed
   into a terminal that logs it):

   ```powershell
   $env:RALPHUS_OUTPOST_TOKEN = '<the secret>'; & 'C:\ralphus\bin\ralphus-outpost.exe' connect http://127.0.0.1:7931 --name winbox --token-env RALPHUS_OUTPOST_TOKEN --remote-root C:/ralphus/fleet
   ```

   The secret travels only on that encrypted session's stdin: it is never on
   a command line or on either machine's disk.

Keep the tunnel and the outpost in the same session: a process the machine's
OpenSSH server starts with `Start-Process` from a session that then ends is
stopped with it, while one in the session's foreground lives exactly as long
as the session. Closing the session (stopping that `ssh`) takes the outpost
down; jobs it started keep running and are picked up when a new session
connects. One session is also much gentler on a Windows OpenSSH server than
the SSH transport's one connection per Machine API call.

## What runs where

Every verb runs inside the outpost process through the machine-side verbs in
`ralphus-runner` (`runner/src/machine/`) -- the same code `ralphus-ssh-provider`
runs on a Windows machine over SSH -- with no shell in between: git is run as
a program, files are read and written directly, an authored command line goes
to `cmd` on Windows and `sh` elsewhere (or the shell the request names).

A cell (`exec`) runs as a durable job under `<remote_root>/jobs/`: the outpost
starts a detached supervisor (`<runner> machine supervise`), which runs the
cell with `<runner> send`. A job outlives the outpost; `status`, `stream` and
`cancel` find it by its handle after a reconnect. A supervisor that stops
beating its heartbeat (every 2 s) for 20 s is reported lost.

Agents use the machine's own installation and login: the outpost does not
install or log in an agent. Give an agent's path in the fleet's
`[machine.agents]` if it is not on the outpost's `PATH`. A command cell that
says `shell = "sh"` needs an `sh` on the machine's `PATH` (Git for Windows'
`usr\bin` provides one).

## Staying connected

The outpost reconnects forever. After a failed attempt it waits with
exponential backoff from 1 s to 60 s, each wait randomised to between half
and all of it so many machines do not return at once. Each long poll is
answered within 20 s, so a connection that stays silent for 30 s is dead and
is replaced; when the wall clock jumps ahead of the monotonic clock (the
machine slept) it starts a fresh session at once instead of waiting out a
backoff. The fleet drops a session it has not heard from for 75 s, failing
its unanswered requests.

## Trust model

- **Who can make the machine do things:** whoever can authenticate as its
  fleet. Requests include `run` (any git command, or any authored shell
  command line in a workspace) and `exec` (any cell), so this is code
  execution on the machine as the user running the outpost. Run the outpost
  as a user with access to nothing but what ralphus should touch.
- **Pairing.** `ralphus-fleet pair` makes an 80-bit one-time code (valid ten
  minutes, single use; 20 failed attempts void every pending code). The fleet
  stores only a key derived from it, never the code. The outpost makes its own
  Ed25519 key pair and sends the public half with an HMAC under the code's
  key; the fleet answers with its own public key, the machine's name and
  `remote_root`, authenticated the same way. Neither the code nor anything an
  eavesdropper could test guesses against cheaply crosses the wire, and an
  answer from anyone who did not issue the code is refused. The code is on
  `install`'s command line, which is fine for a one-time, short-lived value
  that is useless once used.
- **What each side keeps.** The machine keeps its private key and the fleet's
  public key -- its **machine credential** -- in the OS's encrypted per-user
  store: the Windows Credential Manager (DPAPI, under the user's login), the
  macOS Keychain, or the Linux Secret Service. Where no Secret Service is
  reachable (a headless Linux box) it falls back to an owner-only (`0600`)
  file under the home directory, and `install` says so loudly: that file is
  not encrypted at rest. The fleet keeps only the machine's **public** key
  (in the outpost registry) and its own identity key (in the fleet user's OS
  store, or an owner-only file next to the configuration). A copy of the
  fleet's registry lets nobody act as a machine. Nothing secret is ever in a
  configuration file, on a command line or in a log.
- **Both sides authenticate on every connection.** A paired hello trades
  nonces and ephemeral X25519 keys; the fleet signs the transcript with its
  identity key (the outpost takes no request from a service that cannot --
  a spoofed URL or DNS name gets nothing done) and the outpost signs it back
  with its machine key in its first poll (the fleet hands no work to an
  outpost that cannot). With a shared secret, both prove it with
  HMAC-SHA256 over both nonces instead; the secret never crosses the wire.
- **Everything after the handshake is sealed** with ChaCha20-Poly1305 under
  a per-session key (HKDF-SHA256 of the X25519 shared secret, or of the shared
  secret), with the direction and session bound in. Requests carry
  credentials (a job's `execution_environment`, a forge token for `git
  push`), so an observer of plain HTTP sees only ciphertext, and a tampered,
  reflected or replayed message is refused. Paired sessions are forward
  secret: a key stolen later does not open recorded traffic. HTTPS in front
  of the fleet is fine but adds nothing secret-wise.
- **Revocation.** `ralphus-fleet revoke <machine>` deletes the machine's
  public key; the running fleet notices at the machine's next poll, ends its
  session and refuses it from then on. Re-pairing replaces the key the same
  way.
- **Runners** arrive over the sealed channel and are installed only if their
  sha256 matches the hash the fleet announced; a half-downloaded or tampered
  runner is never run.
- **What it does not protect against:** an attacker who controls the fleet
  host (it can already run code on every machine), an attacker who can read
  the machine's credential (malware running as that user, or the `0600`
  fallback file), or someone who controls the machine itself. Traffic
  analysis (timing, sizes) is not hidden. A shared secret is only as strong
  as its length: a captured handshake lets an eavesdropper test guesses
  offline, so both sides refuse one under 32 characters.
- **Credentials reaching jobs** follow the rule every machine follows: in
  the job's process environment only, never in the job directory
  (`docs/machine-provider-service.md#credentials`).

## The wire protocol

All on the fleet's own listener, without the service token:

| Call | Body | Answer |
|---|---|---|
| `POST /v1/outpost/pair` | `{"protocol": "ralphus-outpost-pair-v1", "machine_public_key", "nonce", "mac"}` | `{"name", "remote_root", "fleet_public_key", "mac"}`, or `401` |
| `POST /v1/outpost/hello` | paired: `{"protocol": "ralphus-outpost-v2", "name", "nonce", "ephemeral"}`; secret: `{"protocol": "ralphus-outpost-v1", "name", "nonce"}` | `{"session", "nonce", "ephemeral", "signature"}` (v2) or `{"session", "nonce", "proof"}` (v1), or `401` |
| `POST /v1/outpost/poll` | `{"session", "box": seal({"seq", "info"?, "readiness"?}), "proof"?}` (`proof`: the machine's signature, first poll of a v2 session) | after up to 20 s, `{"box": seal({"seq", "requests": [{"id", "verb", "body", "context"}], "expect": {"agents", "runner"}})}` |
| `POST /v1/outpost/reply` | `{"session", "box": seal({"id", "reply", "events"})}` | `{}` |
| `POST /v1/outpost/runner` | `{"session", "box": seal({"sha256"})}` | `{"box": seal(<runner bytes>)}`, or `409` if it changed |

`seal` is base64 of a random 96-bit nonce and the ChaCha20-Poly1305
ciphertext, with associated data `ralphus-outpost-v1|<direction>|<session>`.
The fleet accepts a session only when the outpost's first sealed poll opens
(and, for v2, its `proof` verifies); each poll's `seq` must grow and is
repeated in the answer, so an old answer cannot be replayed into a newer
poll; each request id runs at most once per session. A new session replaces
the old one, failing that one's unanswered requests. `expect.agents` maps
each agent the outpost should check to its configured path (or null);
`expect.runner` is `{"sha256", "size"}` or null. `GET /v1/outposts` (with the
service token) lists the outpost machines: connected, paired (and the key's
fingerprint), whether a shared secret is set, and the latest readiness
report.

## Same-host outposts and remote checks

An outpost can run on the daemon's own host (a test with its own
`remote_root`, say). That proves the request travelled the Machine API path --
fleet, sealed channel, outpost, machine-side verbs -- and that the daemon did
no remote work itself (machine strict mode still fails any local git or
command the daemon tries on the machine's behalf), but it does **not** prove
the work happened on another computer. [`scripts/fleet_e2e.py`](fleet-e2e.md)
reports which machines were outposts and leaves remoteness to how you placed
them.
