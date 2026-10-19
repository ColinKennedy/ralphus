# ralphus patches to tiny_http 0.12.0

This is tiny_http 0.12.0 as published on crates.io (MIT OR Apache-2.0; see
`LICENSE-MIT` / `LICENSE-APACHE`), wired in through the workspace root's
`[patch.crates-io]`. 0.12.0 is the latest release. Every change from
upstream is listed here; keep this list in sync with the code, and drop a
patch if upstream ships an equivalent fix.

The regression tests that guard these patches live in the ralphus workspace
(`daemon/tests/http_hardening.rs`), because this crate is excluded from the
workspace and its own tests do not run in CI. The unit tests added to
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

## 3. Silenced upstream warnings (`src/lib.rs`)

Upstream compiles with two unused imports and one unused trait. Cargo does
not cap lints for path dependencies, so a crate-level
`#![allow(unused_imports, dead_code)]` keeps them out of every ralphus build.

## Manifest

`Cargo.toml` drops upstream's dev-dependencies. It also declares an empty
`[workspace]` so the crate can be tested standalone.
