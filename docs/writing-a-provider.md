# Writing a machine provider service

A provider service is how machines reach ralphus. Ralphus never adds, finds or
picks a machine: it asks a provider for one that satisfies a request and either
gets an **allocation** or an error saying why not
([`machine-provider-service.md`](machine-provider-service.md) is the contract).
This page walks through writing one, using `ralphus-fleet` (`fleet/`), the
built-in provider, as the worked example. Read the contract first; this page
explains the parts that are easy to get wrong.

## What a provider owns

Everything about machines:

- which machines exist and how to reach them (`ralphus-fleet`: a
  `[[machine]]` list in its own config, reached over SSH);
- what each one is (os, arch, labels -- configured or asked of the machine);
- which machine serves a request, and how many allocations one may hold;
- where projects live on the machine (`remote_root`) and how
  `ralphus-runner` gets there (installed, or uploaded per os/arch);
- which agent executable runs on it;
- what an idle hint means (`ralphus-fleet` only records it; a cloud provider
  might stop the VM).

Ralphus owns the work: squads, reviews, git history, secrets and forge
credentials. A provider never stores a secret; it sees them only in transit
inside an `exec` body.

## The two APIs

A provider serves two HTTP APIs, which may share one listener (as
`ralphus-fleet` does) or not:

1. **The allocation API**, authenticated with the provider's own bearer token
   (`token_env` in the daemon's `[machine.providers.<name>]`):
   `POST /v1/allocations`, `GET`/`DELETE /v1/allocations/{id}`,
   `.../renew`, `.../idle`, `GET /v1/health`.
2. **The Machine API** of each allocation, authenticated with that
   allocation's token: `POST <endpoint>/<op>` for every op in the contract's
   table. The request bodies and reply fields are the provider contract v1
   ones ([`machine-providers.md`](machine-providers.md)), so a provider can be
   a thin layer over an existing v1 program provider.

`ralphus-fleet`'s layout:

| File | What it does |
|---|---|
| `fleet/src/config.rs` | its own config: listen address, token variable, machines; unknown keys are errors |
| `fleet/src/allocations.rs` | allocations kept in a state file (never their tokens), token issue/check, `choose` (selection) and `mismatch` (refusal reasons) |
| `fleet/src/machine_api.rs` | each Machine API op run as the matching `ralphus-ssh-provider` verb; fills in what it owns about the machine |
| `fleet/src/server.rs` | routing, authentication, the allocation API |

## Allocating

`POST /v1/allocations` carries `requirements` (`os`, `arch`, `labels`) and a
`scope` (`squad_id` or `review_id`, plus `key`, the canonical request). Get
these right:

- **Same scope, same allocation.** The daemon may ask again for a scope it
  already holds (a retry after a lost reply). Answer with the existing
  allocation and a fresh token rather than a second machine
  (`Allocations::for_scope`).
- **Refuse with reasons.** When nothing fits, reply `409` with
  `reason_code: "no_matching_machine"` and an `error` that says why each
  candidate did not fit -- ralphus shows it verbatim in the failed squad's
  mailbox message. `ralphus-fleet`'s `mismatch` produces lines like
  `docker runs linux, not windows`. Use `capacity` when matching machines exist
  but are full, and always give a `remediation`.
- **Report what you granted.** `os`, `arch`, `labels` and a `description`
  appear on the board and in `ralphus machine allocations`.
- **Return an `endpoint` the daemon can reach.** `ralphus-fleet` builds it from
  `public_url`, falling back to `http://<listen>`.

## Tokens

- An allocation token is a bearer token for that allocation's Machine API
  only. Compare it in constant time.
- **Do not store tokens.** Persist allocations so the daemon can `GET` and
  `renew` them after either side restarts, but keep tokens in memory: after a
  restart every Machine API call gets `401`, and the daemon renews and retries
  once.
- Keep the previous token valid until it expires when you issue a new one, so
  a call already in flight is not refused.
- `GET /v1/allocations/{id}` returns the allocation without a token; only
  `allocate` and `renew` hand one out.

## Serving the Machine API

- **Echo the contract version.** Every reply carries `"protocol_version": 2`;
  the daemon refuses anything else.
- **Fill in what you own.** The daemon sends `provision` and `cleanup` without
  a `remote_root` and `provision` without a `runner` policy for a provider
  machine; the provider supplies its own. `ralphus-fleet` also sets `exec`'s
  and `preflight`'s `executable` from the machine's `[machine.agents]`
  (`complete_request`).
- **Answer `preflight` if you can.** It is optional, but serving it lets a
  missing agent fail a cell up front with a remedy instead of mid-run: run
  `ralphus-runner preflight --agent <agent> [--executable <path>]` on the
  machine and reply `{"available": true}` or `{"available": false, "detail":
  "<its stderr>"}` (see
  [`machine-provider-service.md`](machine-provider-service.md)).
- **Return events in the reply.** A runner's `RALPHUS_EVENT: {json}` lines
  (stderr, for a v1 program) go into the reply's `events` array, oldest first.
  The daemon records them in Cartographer and enforces the live cost cap from
  them, so dropping them silently disables both.
- **`exec` may be asynchronous.** Return a `handle` and serve `status`,
  `stream` and `cancel` for it, or run synchronously and return `result`. A
  long-running cell should use a handle.
- **`materialize` returns files.** The daemon may run on another host, so
  reply with `files: [{path, content_base64, executable}]` (`path` relative
  and `/`-separated; empty when the source was one file). The daemon refuses a
  path that would leave its destination.
- **Keep secrets off disk and out of argument lists.** `exec` and `run` bodies
  can carry secret environment values: pass them to the job through its
  environment or stdin, never a file on the machine, a command-line argument or
  a log. (`ralphus-ssh-provider` exports them in the launching shell and
  sends scripts on ssh's stdin.)

## Trying it

1. Declare the provider in the daemon's global `config.toml`
   (`[machine.providers.<name>]`, `url`, `token_env`) and set the token
   variable in the daemon's environment.
2. Submit a task with `machine = "<name>"` or a table naming it. A refusal
   shows up as the squad's failure with your reasons; a grant shows up in
   `ralphus machine allocations`.
3. Run [`scripts/fleet_e2e.py`](fleet-e2e.md) against it: point
   `--fleet-config` at a `ralphus-fleet.toml` for `ralphus-fleet`, or adapt the
   script's `Fleet` class to start your service instead.
4. With machine strict mode on, any step ralphus would wrongly run on the
   daemon host fails loudly instead of quietly working, so a green run means
   the work really ran on your machines.
