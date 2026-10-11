# Mailbox delivery: getting messages as they arrive

The daemon puts escalations, failures and watched-entity events in the
**mailbox** (RAL-241). This page covers how a person, a script or an agent
session finds out about new mail, and what each method costs. It also
explains why the MCP server has no live-delivery tool.

Two terms here already mean something else in ralphus:

- A **watch** (RAL-343) is a user's subscription to a squad or review: what
  mail they get. This page is about *how* mail is delivered, whatever the
  subscription.
- **Monitor** is the daemon subsystem that records watches and emits typed
  squad/review events. It is unrelated to Claude Code's `Monitor` tool, which
  appears below as one way to run `ralphus mailbox follow`.

See [`glossary.md`](glossary.md) for both.

## Pull or push

| | `ralphus mailbox check` (pull) | `ralphus mailbox follow` (push) |
|---|---|---|
| What it does | Prints this client's unread messages, then marks them read | Holds one connection open and prints a line as each message is enqueued |
| Returns | Immediately | Never; it runs until killed |
| Marks messages read | Yes | No; it only announces them |
| Covers every message | Yes, everything in the database | No; see [What `follow` does not deliver](#what-follow-does-not-deliver) |
| Daemon cost | One request and a database read per call | One open connection and a heartbeat every 15 s; no database reads while idle |
| Agent cost | One model turn per check, whether or not there is mail | None while idle; one turn per message that wakes the agent |

They work as a pair: `follow` tells you *when* to look, and `check` tells you
*everything* that is waiting.

## Following the mailbox from the CLI

```bash
ralphus mailbox follow
```

- **stdout** gets exactly one line per message:
  `MAILBOX [URGENT] <message>`, `MAILBOX [HIGH] <message>` or
  `MAILBOX [info] <message>`.
- **stderr** gets connection status only (`mailbox follow: connected`,
  `... retrying in Ns`), so anything reading stdout sees nothing but mail.
- It reconnects forever, backing off from 1 s to 30 s between attempts, so it
  survives a daemon restart.
- It is `(read-only-safe)`: it never marks messages read.

Because its output is plain lines, anything that reacts to lines can use it:

```bash
# Desktop notification per message (Linux, notify-send)
ralphus mailbox follow | while read -r line; do notify-send "ralphus" "$line"; done

# Keep a log
ralphus mailbox follow >> ~/ralphus-mail.log
```

Every connection authenticates with the daemon token, through a single-use
ticket from `POST /api/events/ticket` (RAL-222). Wherever `ralphus mailbox
check` works, `follow` does too.

## Agent sessions: `ralphus quick-start watcher`

`ralphus quick-start watcher <backend>` launches an agent session whose job is
to surface mail (see [`cli-reference.md`](cli-reference.md#quick-start-watcher)).
How it learns about new mail depends on the backend:

| Backend | Delivery | Cost while idle |
|---|---|---|
| `claude-code` | **Push.** The session is told to run `ralphus mailbox follow` once under Claude Code's `Monitor` tool. Each printed line wakes the session, which then runs `ralphus mailbox check` and shows the result. | None |
| `codex` | **Poll every turn.** After each user turn the session runs `ralphus mailbox check`. | One check per turn; an idle session never wakes up on its own |
| `pi` | **Poll every turn**, same as `codex`. | Same as `codex` |

Codex and Pi poll because neither gives a ralphus-launched process a way to
start a turn in an interactive session today. See
[Possible future work](#possible-future-work).

## What `follow` does not deliver

`follow` only gives the "free" behaviour for the messages that actually reach
the stream. Know its limits before relying on it alone.

1. **Only Monitor events are pushed.** A message goes out on the stream only
   when the daemon raises it through `Store::notify_watchers*`
   (`daemon/src/monitor.rs`). These are squad/review status and failure
   events, waypoint events and follow-up offers. Mail raised directly through
   `enqueue_mailbox_message*` / `enqueue_error_mailbox_message*` is stored but
   **not pushed**. Today that includes:
   - PR CI/merge-blocker notices (`ci_watch.rs`, the RAL-375 `review`
     category)
   - cell stall and CPU-stall escalations (`runner.rs`, `cpu_stall.rs`)
   - forge and daemon-thread health (`forge_health.rs`, `thread_health.rs`)
   - review merge/worktree-claim problems (`guardian_merge.rs`, `pr.rs`)
   - Ark notices (`ark.rs`)
   - the scheduler's direct error path (`scheduler.rs`)

   An idle `follow` session never wakes for these. They still show up on the
   next `mailbox check`.
2. **No replay.** The stream is in memory. A message enqueued while `follow`
   is disconnected, during a reconnect or a daemon restart, is never pushed.
3. **Overflow drops.** Each stream connection buffers 256 events. A reader
   that falls that far behind silently misses events until it catches up.
4. **It never marks anything read.** Use `check` (or `mailbox personal-drain`)
   for that.

The rule that covers all four: **whenever `follow` wakes you, run `mailbox
check`**, which drains everything, pushed or not. The Claude Code watcher
already does this. A long-running session should also run `check` now and
then, or after a reconnect, so mail that never reached the stream isn't left
waiting.

## Why the MCP server has no `mailbox_follow`

`ralphus-mcp` exposes nearly every CLI command as an MCP tool (see
[`mcp-server.md`](mcp-server.md)). `mailbox follow` is one of the few that is
deliberately left out, in `mcp/src/exclusions.rs`, and calling it through the
server returns a usage error. The reasons:

- **An MCP tool call is one request and one response.** `follow` never
  returns, so it has no response to give. A tool that blocked until the first
  message arrived would freeze the agent's turn until then.
- **The server never speaks first.** `ralphus-mcp` advertises only the `tools`
  capability and only replies to requests (`mcp/src/protocol.rs`). Standard
  MCP has no message that makes the host start a new turn in the agent's
  session, so even a server-side notification would not wake an idle agent.
  (Sampling lets a server request a one-off completion, which is not a turn
  in the session.)
- **Polling through MCP is the expensive option this page is avoiding.**
  Calling `mailbox_check` in a loop costs a daemon request and a model turn
  every time, mail or not. The MCP `listen` tool is not a substitute: it polls
  a squad's *status*, not the mailbox.

### What to do instead

Combine them: let the CLI wake the session and use MCP tools to act.

- **Claude Code:** run `ralphus mailbox follow` under the `Monitor` tool, which
  is what `quick-start watcher claude-code` sets up. When a line arrives, call
  `mailbox_check` (MCP) or `ralphus mailbox check` (CLI) and handle it with
  whichever tools you like.
- **Any MCP host without a background-process tool:** call `mailbox_check` at
  natural turn boundaries, as the Codex and Pi watchers do. It costs a turn
  per check, but it is reliable.

## Possible future work

None of these are implemented. They are recorded so the next person starts
from what is known.

### Claude Code: Channels in `ralphus-mcp`

Claude Code has a research-preview extension called
[Channels](https://code.claude.com/docs/en/channels-reference) that lets an
MCP server push into a session. The server declares
`capabilities.experimental["claude/channel"] = {}` and sends
`notifications/claude/channel` with `{ content, meta }`. The event arrives as a
`<channel source="...">` block and starts a turn.

A `--channel` mode for `ralphus-mcp` would:

1. Add the capability and an `instructions` string to `initialize`.
2. Run the `follow` loop on a background thread, refactoring
   `cli/src/commands/mailbox.rs`'s `follow_once` to hand notices to a
   callback instead of printing, because stdout is the MCP wire.
3. Send each notice as a channel notification. `meta` keys may contain only
   letters, digits and underscores; other keys are dropped.
4. Put stdout behind a lock so replies and notifications cannot interleave.

Caveats:

- Each launch needs `claude --dangerously-load-development-channels
  server:ralphus` and a confirmation dialog.
- Interactive sessions only; `-p` and the Agent SDK ignore the flag.
- Team/Enterprise orgs must enable channels.
- Events from a server the session didn't load as a channel are dropped
  silently.

Because the `Monitor` route already gives zero-idle-cost push without any of
this, Channels is mainly worth building for MCP-only setups.

### Pi: a bundled mailbox extension

Pi's extension API can start a turn from background code:
`pi.sendMessage(message, { triggerTurn: true, deliverAs })` runs a new agent
turn when the session is idle. While it is busy, `deliverAs` queues the
message instead: `"steer"` before the next model call, `"followUp"` after the
current work, `"nextTurn"` with the user's next prompt.

ralphus already bundles one Pi extension, the workspace guard installed by
`runner/src/pi_backend.rs`, so a second one would:

- on `session_start`, spawn `ralphus mailbox follow` as a child process;
- call `pi.sendMessage(..., { triggerTurn: true })` per line;
- kill the child on `session_shutdown`.

`quick-start watcher pi` would then switch from poll-every-turn to push. This
is the most direct of the three routes. It spawns a process, so it also needs
a `proc_counts/tags.toml` entry ([`proc-counts.md`](proc-counts.md)).

### Codex: no clean route

Codex's hooks (`SessionStart`, `UserPromptSubmit`, `PreToolUse`,
`PostToolUse`, `Stop`) all fire from Codex's own lifecycle. None fires while
the session is idle, and no documented mechanism lets an outside process start
a turn in the interactive TUI.

The one workaround is a `Stop` hook that blocks until `follow` prints a line,
then returns `{"decision": "block", "reason": "<mail>"}` so Codex continues
with the mail as its next prompt. It has serious limits:

- The session never hands control back, so it only suits a dedicated watcher
  session nobody types into.
- Hook timeouts fail open, which ends the session.
- It is untested whether Codex still honours a block once `stop_hook_active`
  is set.

Hosting the watcher through Codex's `app-server` protocol might allow
ralphus to start turns itself; that has not been investigated.

## Implementation map

| Piece | Where |
|---|---|
| In-memory event bus, `MailboxNotice`, `publish_mailbox` | `daemon/src/events.rs` |
| Messages that publish to the bus | `Store::notify_watchers*`, `daemon/src/monitor.rs` |
| Messages that do not | `Store::enqueue_mailbox_message*` / `enqueue_error_mailbox_message*`, `daemon/src/mailbox.rs` |
| `GET /api/events` SSE stream, 15 s heartbeat | `serve_events_stream`, `daemon/src/server.rs` |
| Stream tickets | `daemon/src/token.rs` |
| `mailbox follow` / `mailbox check` | `cli/src/commands/mailbox.rs` |
| Watcher delivery per backend | `WatcherDeliveryMechanism`, `cli/src/commands/quick_start.rs` |
| MCP exclusion of `follow` | `mcp/src/exclusions.rs`, `mcp/src/exec/mailbox.rs` |
