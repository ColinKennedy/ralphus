# STRESS-002: one connection holding a request body freezes the whole daemon (no auth needed)

- **Severity:** critical. One unauthenticated, low-bandwidth client can
  block every request indefinitely.
- **Found by:** `ralphus-stress`, fault `truncated_body_hold`, confirmed
  with a standalone experiment
- **Found at:** `035ef1e70` (staging, 2026-10-18), release build, Windows 10
- **Status:** open

## What happens

A client sends the headers of a POST with `Content-Length: 1048576` and
64 bytes of body, then keeps the socket open. **No other request is
answered** until that client sends the rest or closes.

| Measurement | Result |
|---|---|
| `GET /api/daemon` while one such connection was held, with a valid token | timed out after 15 s |
| Same, with no `Authorization` header | timed out after 15 s |
| `GET /api/daemon` after the holder closed | 0.02 s |
| Victim load (16 users) during a 13 s hold, from the stress fault | no request completed for 11.4 s; 18% errors; throughput 4 ok/s against 17 at baseline |

## Root cause

- **One thread reads every request body.** `run_http_loop`
  (`daemon/src/server.rs:21067`) is the only thread draining
  `server.recv()` (`server.rs:21071`). It reads the full body *before*
  dispatching to the read or write pools: `request.as_reader().read_to_string(&mut body)`
  at `server.rs:21186-21187`.
- **Bodies arrive lazily.** tiny_http's per-connection thread parses only
  the headers and hands `recv()` a lazy reader over the socket
  (`tiny_http-0.12.0/src/client.rs:106-168`, `request.rs:215`), so the read
  blocks on the slow client.
- **Nothing times out.** No socket read timeout is set: the daemon builds the
  listener with plain `Server::http` (`server.rs:19997`, `20348`), and
  tiny_http sets none itself.
- **Auth comes too late to help.** The bearer check is in
  `answer_request` (`server.rs:20657`), after the body read.
- **Chunked bodies stall the same way.** A trickled chunked body
  (`Transfer-Encoding: chunked`) does the same. That is inferred from the
  code and was not run.

## Fix direction

- **Move the body read off the accept loop** into the pool worker that
  handles the request. The accept loop should only route by method and URL.
  This alone still lets a holder pin the *single* write worker
  (`WRITE_WORKERS = 1`, `server.rs:20524`), so it is not enough by itself.
- **Add a read deadline.** Bound the total time to read headers and body
  (for example a few seconds per MiB with a floor), and on expiry answer
  `408` and close. tiny_http exposes no timeout setting. Options are to wrap
  the socket via a custom listener, read the body on a helper thread with a
  deadline, or move to a server crate that has per-connection timeouts.
- **Check the bearer token from the headers before reading the body.** An
  unauthenticated client should never get to make the daemon wait.
- **Combine with STRESS-001's body-size cap.**

## Regression test

Add it to `daemon/tests/http_concurrency.rs`:

1. Open a raw connection, send headers with `Content-Length: 1048576` and a
   few body bytes, and hold the socket.
2. Assert that a concurrent `GET /api/daemon` answers within a short bound
   (for example 2 s).
3. Repeat with no token.

## Reproduce

```bash
target/release/ralphus-stress run --daemon-exe target/release/ralphus-daemon \
  --scenarios faults --faults truncated_body_hold --fault-secs 15 --out /tmp/stress-out
# verdict "fail ... server stalled: no victim request completed for ~11s"
```
