# MCP server (RAL-301)

`ralphus-mcp` exposes ralphus's whole command surface — squads, tasks, cells,
proofs, reviews, queues, projects, machines, agents — as **MCP tools**, so an
MCP client (Claude Code, or any other agent host that speaks MCP) can drive
ralphus directly instead of shelling out to the `ralphus` CLI.

It talks to the daemon's HTTP API through `ralphus-cli`'s `DaemonClient`,
reused as a library. It never invokes the compiled `ralphus` binary, so the
CLI does not need to be installed or on `PATH` for the MCP server to work —
only a reachable daemon.

> **Status:** implemented and covered by `mcp/tests/parity.rs`, but
> **build-from-source only**. `scripts/build-release.sh` does not build
> `ralphus-mcp`, and `scripts/build-bundle.py` does not ship it — the
> distributable bundle contains `ralphus-daemon`, `ralphus-librarian`,
> `ralphus`, and `ralphus-runner` only. Build it yourself as below.

## Setup shortcut: `ralphus mcp initialize`

After `ralphus-mcp` has been built or installed, the CLI can configure a
supported agent host for you:

```bash
ralphus mcp initialize <claude|codex|pi> [--profile-file <path>] [--dry-run] [--yes]
```

This is a shortcut for the host-specific registration and PATH setup below.
It finds `ralphus-mcp`, registers it with the selected host, and adds the
binary's directory to the selected shell profile when needed. It does **not**
build `ralphus-mcp`; complete the build step first, or set
`RALPHUS_MCP_PROGRAM` to the executable's absolute path.

Start by previewing the exact changes:

```bash
ralphus mcp initialize codex --dry-run
```

Run the same command without `--dry-run` to confirm and apply the plan. Use
`--yes` for a non-interactive apply, or `--profile-file <path>` to choose a
profile instead of the shell-derived default. For Pi, the plan may also
install the third-party Pi MCP Adapter; inspect its publisher and source in
the dry-run output before applying it.

## Quickstart: Claude Code

**1. Build the binary**

```bash
cargo build --release -p ralphus-mcp
```

**2. Copy it somewhere stable — outside `target/`**

```bash
cp target/release/ralphus-mcp.exe dist/ralphus-mcp.exe   # Windows
cp target/release/ralphus-mcp    dist/ralphus-mcp        # Linux/macOS
```

This step is not cosmetic. An MCP client keeps the server running as a
long-lived subprocess, and on Windows a running executable is locked against
rewriting — so pointing the client at `target/debug/` or `target/release/`
means the next `cargo build` fails to relink that binary. It is the same
class of failure as the running-daemon lock described in
[`.agent/gotchas.md`](../.agent/gotchas.md). `dist/` is outside cargo's
output tree, so cargo never tries to write the file the client is holding
open.

**3. Register it with Claude Code**

```bash
claude mcp add ralphus -s local -- C:\path\to\ralphus\dist\ralphus-mcp.exe
```

Use an **absolute path**. The client launches the server with an unspecified
working directory, so a relative path will not resolve.

**4. Verify**

```bash
claude mcp list
```

`ralphus: ...\dist\ralphus-mcp.exe - ✔ Connected` means the handshake
succeeded. Then **restart Claude Code** — a session's tool list is fixed at
startup, so a server added mid-session is not visible until the next launch.
After restarting, `/mcp` lists the server and its tools.

## Choosing a scope

`claude mcp add -s <scope>` decides where the registration is written:

| Scope | Stored in | Use when |
|---|---|---|
| `local` *(default)* | `~/.claude.json`, under this project's entry | **Normal choice here.** Private to you, and scoped to the ralphus checkout you registered it from. |
| `user` | `~/.claude.json`, globally | You want the ralphus tools available from every directory, not just the checkout. |
| `project` | `.mcp.json` at the repo root, committed | Shared with everyone who clones the repo. |

`project` scope is a poor fit for this server: `.mcp.json` is committed, but
the server's path is an absolute, machine-specific path to a binary each
developer must build themselves. Prefer `local`.

Note that `.mcp.json` must be at the **repository root** to be read. A file
at `.claude/mcp.json` is not a config path Claude Code loads, and servers
defined there are silently inert.

## Registering with other MCP hosts

The server speaks newline-delimited JSON-RPC 2.0 over **stdio**. Any host
that supports stdio MCP servers can launch it with the standard shape:

```json
{
  "mcpServers": {
    "ralphus": {
      "command": "C:\\path\\to\\ralphus\\dist\\ralphus-mcp.exe",
      "args": []
    }
  }
}
```

Implemented methods are `initialize`, `tools/list`, `tools/call`, and `ping`.
Notifications (such as `notifications/initialized`) are accepted and produce
no reply, per the JSON-RPC spec. There are no `resources/*` or `prompts/*`
methods — a host that probes for them gets a `-32601 method not found`, which
is expected and harmless.

## Connecting to the daemon

The server needs a running daemon, and (since RAL-219) a bearer token for it.
Both are resolved automatically for a normal local setup, so the zero-config
registration above is usually all you need.

| What | Default | Override |
|---|---|---|
| Daemon URL | `http://127.0.0.1:7890` | `--daemon-url <url>` flag, or `RALPHUS_DAEMON_URL` |
| Bearer token | read from `~/.ralphus/daemon.token` | `RALPHUS_DAEMON_TOKEN` (wins over the file) |

The token file is written by the daemon at startup, so a locally-run daemon
needs no configuration at all. Set `RALPHUS_DAEMON_TOKEN` when pointing at a
**remote** daemon, where the MCP server has no filesystem access to that
host's token file:

```bash
claude mcp add ralphus -s local \
  -e RALPHUS_DAEMON_TOKEN=... \
  -- /path/to/ralphus-mcp --daemon-url http://ralphus-host:7890
```

## Read-only mode

Passing `--read-only` restricts the server to tools tagged `read_only_safe`
in `cli/src/help_map.rs` — roughly 56 of the ~130 tools. Restricted tools are
not merely refused on call; they are absent from `tools/list`, so the client
never offers them. This mirrors `ralphus quick-start --read-only` (see
[`cli/AGENTS.md`](../cli/AGENTS.md)'s Read-Only Quick-Start Safety List) and
is the right registration for a session that should be able to inspect
squads, reviews and logs but never submit, clear, merge or promote.

Registering both is reasonable — they are independent servers:

```bash
claude mcp add ralphus    -s local -- C:\path\to\dist\ralphus-mcp.exe
claude mcp add ralphus-ro -s local -- C:\path\to\dist\ralphus-mcp.exe --read-only
```

## Using the tools

In Claude Code the tools appear under the `mcp__<server>__<tool>` naming
convention — with the server registered as `ralphus`, that is
`mcp__ralphus__squad_list`, `mcp__ralphus__task_show`,
`mcp__ralphus__review_status`, and so on. The tool name is the CLI path with
spaces replaced by underscores: `ralphus squad list` → `squad_list`.

The tool list is generated mechanically from
`help_map::registered_leaves()`, so it tracks the CLI automatically — a new
leaf gets a tool and a JSON Schema for free. It does **not** get execution
for free; see [`mcp/AGENTS.md`](../mcp/AGENTS.md).

Because the list is generated at build time, the running server reflects the
binary you registered. **After adding CLI commands, rebuild and re-copy to
`dist/`**, then restart the client.

## Verifying without a client

The stdio protocol is plain text, so the server can be exercised directly —
useful for telling "the server is broken" apart from "the client is
misconfigured":

```bash
printf '%s\n%s\n' \
  '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"probe","version":"0"}}}' \
  '{"jsonrpc":"2.0","id":2,"method":"tools/list","params":{}}' \
  | dist/ralphus-mcp.exe
```

A healthy server replies with its `serverInfo` and then the full tool array.
Swap the second line for a `tools/call` to prove it reaches the daemon:

```json
{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"squad_list","arguments":{}}}
```

## Troubleshooting

| Symptom | Cause |
|---|---|
| `✗ Failed to connect` in `claude mcp list` | Wrong or relative path to the executable, or it was never built. |
| Connects, but every tool call errors with `unauthorized` | The daemon is up but the token did not resolve. Check `~/.ralphus/daemon.token` exists, or set `RALPHUS_DAEMON_TOKEN`. |
| Connects, but tool calls fail with a connection error | No daemon is listening on the URL. Start one, or pass `--daemon-url`. |
| Server registered but no tools in the session | The session was already running when it was added. Restart the client. |
| Mutating tools missing | The server was registered with `--read-only`. |
| `cargo build` fails to relink `ralphus-mcp` | The client is holding the binary open. Register the copy in `dist/`, not the one in `target/`. |

## See also

- [`mcp/AGENTS.md`](../mcp/AGENTS.md) — module map, the parity test, and how
  to wire up a newly-added CLI command.
- [`docs/daemon-api.md`](daemon-api.md) — the HTTP API underneath.
- [`docs/cli-reference.md`](cli-reference.md) — the command surface these
  tools mirror.
