# Benchmark Harness (RAL-94)

A custom, opt-in harness that durably times every `#[ralphus_bench]`-tagged
Rust test, one commit at a time, so per-test timing regressions are visible
per commit instead of only "the whole suite got slow" after the fact. It
never runs as part of normal `cargo test` — it is a separate, explicit
invocation, split across two independent steps: generating data, and
rendering graphs from it.

## Step 1 — Generate data (runs tests, writes JSON)

```bash
cargo run -p ralphus-bench-harness --bin ralphus-bench-rs
```

Implements the "durable minimum" stopping rule: run the test once, in-process;
if it beat the best duration seen so far, remember it and reset `patience`;
otherwise decrement `patience`; stop at `patience == 0`. Every raw sample is
kept and reduced to a full stats bundle (`max`, `mean`, `median`, `stddev`,
`iqr`, Tukey `outliers`) alongside `durable_min`. Results accumulate one
record per commit per test under `bench_data/rust/` — see
`bench-harness/src/storage.rs`.

Adding a crate's tests to the harness is a deliberate, visible, per-test
action: opt a test in with `#[ralphus_bench(patience = N)]` (see
`ralphus-bench-macros`), which expands to the original function, a
separately-named `#[test]`-tagged wrapper that calls it once (so it keeps
running under plain `cargo test` too), and a `const BenchMeta` describing it.
The crate then hand-maintains a `pub fn ralphus_bench_tests() -> Vec<BenchMeta>`
collector (see `core/src/bench_demo.rs` for the reference example) that
`ralphus-bench-rs` calls into. This is the only way to reach into an ordinary
`#[test]` function and re-invoke it in-process, repeatedly, without either
linker-level magic (rejected: this workspace forbids `unsafe_code`) or moving
every test out of `#[cfg(test)] mod tests` (rejected: too large and risky a
mechanical migration to do blind) — `cargo test`'s implicit lib-unittest
target gives no way to swap out or disable its own harness.

## Step 2 — Render graphs (reads already-stored data, no test execution)

```bash
cd cli && uv run ralphus-bench-graph --lang rust
```

This is a small standalone Python tool (`cli/src/ralphus/bench/` —
`storage.py` + `graphs.py`, both stdlib-only) kept specifically for this
step; it never runs or times a test itself, it only reads whatever JSON is
already on disk under `bench_data/rust/` and writes SVG/HTML. Running the
data-generation command from step 1 does **not** trigger this step
automatically — re-run it any time you want the graphs to reflect newly
recorded data.

It writes a standalone HTML page a developer opens directly —
`bench_data/index.html` — not raw `.svg` files, and not part of `board.html`
or the public docs site. That page links to a per-language landing page
(`bench_data/<language>/index.html`), which links to a per-file/module page
(`bench_data/<language>/.../index.html`) that embeds that group's summary,
multiline, and per-test SVGs via `<img>`. Everything is plain string-built
SVG — no charting library, no new dependency.

Three graph types per file/module group:

| Graph | Contents |
|---|---|
| Per-test | One line, one test, x = commit, y = `durable_min` |
| Per-file multiline | Every test in the group on one plot; each series labeled with a letter drawn on the plot (color is hashed from the test's identity, since cardinality is unbounded), legend below maps letters to test names |
| Per-file summary | min / median / mean / max of `durable_min` aggregated across the group's tests per commit; fixed 4-series categorical palette |

A skipped test (Rust has no runtime equivalent — a crate's hand-maintained
`ralphus_bench_tests()` collector simply omits a test it doesn't want
benchmarked) is recorded explicitly as a `skipped` entry alongside `records`,
rather than simply being absent. Graph rendering only ever reads `.records`,
so a skip is surfaced as a count and never plotted as a trend-line point.

## Authoring rule — patience

Every time you add a benchmarked test (`#[ralphus_bench(...)]`), deliberately
consider whether the default patience (10) is appropriate. If you explicitly
set a patience value — even if it happens to equal the default — the line
must carry a trailing inline comment explaining why:

```rust
#[ralphus_bench(patience = 3)] // low: validating an empty file is trivial and deterministic — few improving runs needed to find its floor
fn validate_toml_rejects_empty_file() { ... }
```

Tests using the implicit default (no `#[ralphus_bench]` argument at all) need
no comment. `scripts/check_bench_patience_comments.py` enforces this
mechanically for every tracked `*.rs` file.

## Where filenames come from

Test-name-derived filename stems are hashed (not embedded verbatim) once they
run long, since Windows' `MAX_PATH` is 260 chars and a test's full path can
blow past a safe budget on its own — see `_short_identifier` in
`cli/src/ralphus/bench/storage.py` (shared by the Python-side record
read/write helpers `graphs.py` and `bench-harness/src/storage.rs` both key
their JSON filenames the same way).
