# ralphus patches to tiny_http 0.12.0

This is tiny_http 0.12.0 as published on crates.io (MIT OR Apache-2.0; see
`LICENSE-MIT` / `LICENSE-APACHE`), wired in through the workspace root's
`[patch.crates-io]`. 0.12.0 is the latest release. Every change from
upstream is listed here; keep this list in sync with the code, and drop a
patch if upstream ships an equivalent fix.

## Pristine base

| | |
|---|---|
| **Version** | `tiny_http` 0.12.0 |
| **crates.io checksum (sha256)** | `389915df6413a2e74fb181895f933386023c71110878cd0825588928e64cdc82` |

To see exactly what ralphus changed, download the 0.12.0 `.crate` (or take
it from `~/.cargo/registry/cache/`), confirm its sha256 matches the value
above, unpack it, and diff it against this directory. This copy is
deliberately not a git submodule: upstream has not released in years, so
there is little to merge, and the patches below are maintained here.

The regression tests that guard these patches live in the ralphus workspace
(`daemon/tests/http_hardening.rs`). This crate is excluded from the
workspace, so CI runs its own unit tests through a separate step
("Test (vendored tiny_http)"). The unit tests added to
`src/util/equal_reader.rs` document the behaviour; run them with
`cargo test --manifest-path vendor/tiny_http/Cargo.toml --lib` if you change
it.

## 1. Bounded body drain (`src/util/equal_reader.rs`)

| | |
|---|---|
| **Found by** | `ralphus-stress`, STRESS-001 (`perf/stress/followups/`) |
| **Problem** | `EqualReader::drop` drained an unread body with `vec![0; remaining]`. A request declaring `Content-Length: 99999999999999` and sending two bytes made the whole process abort on allocation failure (`0xc0000409` on Windows), without authentication. |
| **Change** | Drain through a fixed 8 KiB buffer. Drain at most `MAX_DRAIN` (1 MiB); a longer remainder is left unread. The connection's next parse then fails and the connection closes. |

## 2. Request-line and header limits (`src/client.rs`)

| | |
|---|---|
| **Found by** | `ralphus-stress`, STRESS-009 |
| **Problem** | Request lines and headers were buffered without bound. A non-ASCII request line closed the connection without any response. |
| **Change** | See the table below. |

| Condition | Response |
|---|---|
| Request line longer than `MAX_REQUEST_LINE` (64 KiB) | `414` |
| One header line longer than `MAX_HEADER_LINE` (64 KiB) | `431` |
| All header lines together longer than `MAX_HEADER_BYTES` (128 KiB) | `431` |
| More than `MAX_HEADER_COUNT` (256) header lines | `431` |
| Non-ASCII request line | `400` |
| Non-ASCII header line | `400` |

In every case the connection is closed afterwards. The limits are far above
anything the board, CLI or MCP server sends.

## 4. Per-connection read timeout (`src/lib.rs`, `src/client.rs`, `src/connection.rs`)

| | |
|---|---|
| **Found by** | `ralphus-stress`, STRESS-002 |
| **Problem** | No socket read timeout existed. A client that sent headers, or part of a body, and then stopped could hold its connection thread (and, through ralphus's accept loop, every request) forever. |
| **Change** | New `Server::set_read_timeout(Option<Duration>)`, `None` by default. See the table below. |

| Situation once a timeout is set | Behaviour |
|---|---|
| Any socket read waits that long | It fails with a timeout (`TimedOut`, or `WouldBlock` on Unix). |
| Request line and headers still unread that long after the request's first byte | Answered `408`, then closed. This catches headers trickled slowly enough that no single read times out. |
| Idle keep-alive connection: nothing received before the first byte of a new request | Closed without a response. |

Writes are never affected, so streamed responses (the daemon's
`/api/events`) keep working. Clients that pool connections cope with
idle closes: ureq checks `server_closed()` before reusing one.

## 5. Connection threads not double-booked (`src/util/task_pool.rs`)

| | |
|---|---|
| **Found by** | The RAL-414 board cold-load CI job, stalling ~30 s on a Linux runner |
| **Problem** | `TaskPool::spawn` queued a new connection whenever `waiting_tasks > 0`. A thread that has been notified stays counted there until it wakes, so two connections accepted back to back could both be queued for one idle thread. The second then waited, unread, until a keep-alive connection closed and freed a thread: never without a read timeout, 30 s with patch 4. A browser opening several connections at once hit it on a busy 2-CPU machine. |
| **Change** | Spawn a new thread unless there are more idle threads than queued tasks (`waiting_tasks > queue.len()`). |

## 3. Silenced upstream warnings (`src/lib.rs`)

Upstream compiles with two unused imports and one unused trait. Cargo does
not cap lints for path dependencies, so a crate-level
`#![allow(unused_imports, dead_code)]` keeps them out of every ralphus build.

## Manifest

`Cargo.toml` drops upstream's dev-dependencies. It also declares an empty
`[workspace]` so the crate can be tested standalone.
