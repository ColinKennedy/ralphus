# STRESS-001: one request with a huge `Content-Length` aborts the daemon (no auth needed)

- **Severity:** critical (remote, unauthenticated crash of the whole daemon)
- **Found by:** `ralphus-stress`, fault `malformed_requests`, payload
  `huge_content_length_then_close`
- **Found at:** `035ef1e70` (staging, 2026-10-18), release build, Windows 10
- **Status:** open

## What happens

This request kills the daemon process outright:

```
POST /api/squads HTTP/1.1
Host: x
Content-Length: 99999999999999

{}
```

The client then closes the socket. The daemon exits with `0xc0000409`
(the fast-fail abort Rust uses for an allocation failure). Its stderr says:

```
memory allocation of 99999999999997 bytes failed
note: run with `RUST_BACKTRACE=1` environment variable to display a backtrace
```

No `Authorization` header is needed.

## Root cause

- **The allocation is in tiny_http, not ralphus.**
  `tiny_http-0.12.0/src/util/equal_reader.rs:67-70`, in `EqualReader::drop`,
  drains the unread rest of a body with `let mut buf = vec![0; remaining_to_read];`.
  `remaining_to_read` is the declared `Content-Length` minus the bytes
  already read, so 99999999999999 − 2 for `{}`.
- **Bodies over 1024 bytes are read lazily.** They get a lazy
  `FusedReader(EqualReader(..))` (`tiny_http-0.12.0/src/request.rs:215-216`).
  When the socket hits EOF, `FusedReader` drops the inner reader
  (`util/fused_reader.rs:27-28`), which triggers the drain above. Dropping a
  `Request` with an unread oversized body does the same
  (`request.rs:52`, `request.rs:488`).
- **The daemon reads the body before checking auth.** The read is in the
  accept loop at `daemon/src/server.rs:21186-21195`
  (`run_http_loop` → `request.as_reader().read_to_string(&mut body)`). The
  bearer-token check comes later, in `answer_request` at
  `daemon/src/server.rs:20657`. Only the CORS-deny path, `OPTIONS` and the
  `/api/events` ticket check run first (`server.rs:21088-21178`).
- **There is no body-size limit anywhere.** The daemon has no `MAX_BODY`,
  no 413, and no `.take()` on the request reader.
- **The librarian has the same unbounded read** at
  `librarian/src/server.rs:614`, so it can presumably be crashed the same way.
  That is inferred from the code and was not run. It forwards a correct
  `Content-Length` to the daemon, so the daemon cannot be crashed *through* it.

## Fix direction

1. **Rejecting with 413 in ralphus is not enough.** Dropping the request still
   runs `EqualReader::drop` and allocates. Make the drain bounded: either
   vendor/patch tiny_http (`[patch.crates-io]`) so `EqualReader::drop`
   drains through a fixed buffer
   (`io::copy(&mut (&mut self.reader).take(self.size as u64), &mut io::sink())`),
   or check whether a newer tiny_http already fixes it.
2. In `run_http_loop` (`daemon/src/server.rs:21067`):
   - Check `request.body_length()` (`tiny_http request.rs:279`). Above a cap
     (e.g. 16 MiB), answer `413` with `Connection: close` and skip the
     handler.
   - Otherwise read through `.take(CAP + 1)` and answer `413` on overflow.
     This also covers chunked bodies, which have no declared length.
3. Make the same change in `librarian/src/server.rs:614`.

## UX risk: low

- **Large task files.** A legitimately huge task file (giant prompts, pasted
  logs) would get `413` instead of being accepted.
  - Set a generous cap (16–64 MiB). Today the largest real submits are far
    smaller; the stress run's 64 MiB padded submit was accepted.
  - Make the New Squad modal show the `413` as a readable error ("task file
    too large: N MiB, limit M MiB"), not a generic failure toast.
- **The librarian.** Use the same cap there, so the daemon and the
  librarian never disagree about what is too big.

## Regression test

Add it to `daemon/tests/http_concurrency.rs`, or to
`daemon/tests/api_over_http.rs` using its `serve_with_token` pattern. Over a
raw `TcpStream`:

- Send the request above, then close the socket. Assert the daemon still
  answers a following `GET /api/daemon`.
- Send an oversized `Content-Length` and assert `413`.
- Run both with and without a token.

## Reproduce

```bash
cargo build --release -p ralphus-daemon -p ralphus-stress
target/release/ralphus-stress run --daemon-exe target/release/ralphus-daemon \
  --scenarios faults --faults malformed_requests --out /tmp/stress-out
# the malformed_requests verdict is "fail: payload "huge_content_length_then_close" killed the daemon"
```
