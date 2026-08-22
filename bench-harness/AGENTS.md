# bench-harness/ (and bench-macros/, bench-types/)

## RAL-94 benchmark harness

A custom, **opt-in** benchmark harness durably times every `#[ralphus_bench]`-
tagged Rust test, one commit at a time, so per-test timing regressions are
visible per commit instead of only "the whole suite got slow" after the fact.
It is never run as part of normal `cargo test` — it is a separate, explicit
invocation, split across two independent steps:

```bash
# Generate data: benchmarks every #[ralphus_bench]-tagged test, writes JSON
cargo run -p ralphus-bench-harness --bin ralphus-bench-rs

# Regenerate the SVG + HTML graphs from already-stored data (no test execution)
cd cli && uv run ralphus-bench-graph --lang rust
```

`ralphus-bench-rs` only *runs tests and writes JSON* to `bench_data/rust/` —
it never touches the SVGs/HTML. `ralphus-bench-graph` (a small standalone
Python tool, `cli/src/ralphus/bench/` — see `cli/AGENTS.md`) only *reads*
already-stored JSON and (re)renders graphs — it never executes a test. After
running the data-generation command you must re-run `ralphus-bench-graph` to
see updated graphs — it is not triggered automatically. See
[`docs/bench-harness.md`](../docs/bench-harness.md) for the full developer
walkthrough.

Implements the "durable minimum" stopping rule: run the test once, in-process;
if it beat the best duration seen so far, remember it and reset `patience`;
otherwise decrement `patience`; stop at `patience == 0`. Every raw sample is
kept and reduced to a full stats bundle (`max`, `mean`, `median`, `stddev`,
`iqr`, Tukey `outliers`) alongside `durable_min`. Results accumulate one
record per commit per test under `bench_data/rust/` — see
`bench-harness/src/storage.rs`. Test-name-derived filename stems are hashed
(not embedded verbatim) once they run long, since Windows' `MAX_PATH` is 260
chars and a test's full path can blow past a safe budget on its own.

A skipped test is recorded explicitly — a `skipped` entry alongside `records`
in that test's JSON file — rather than simply being absent; graph rendering
only ever reads `.records`, so a skip is surfaced as a count and never
plotted as a trend-line point. There's no runtime equivalent for this today:
a crate's hand-maintained `ralphus_bench_tests()` collector simply omits a
test it doesn't want benchmarked, which is already an intentional per-test
opt-in decision rather than a runtime skip.

`cargo test`'s implicit lib-unittest target (the one that runs
`#[cfg(test)] mod tests` inline in `src/`) is the thing that executes the
vast majority of this repo's Rust tests, and Cargo gives no way to swap out
or disable that target's harness — `harness = false` only applies to a
`[[test]]` entry pointing at a file under `tests/`, not to the implicit
inline-unittest target. There is therefore no way for an external
`ralphus-bench-rs` binary to reach into an ordinary `#[test]` function and
re-invoke it in-process, repeatedly, without either linker-level magic
(rejected: this workspace forbids `unsafe_code`) or moving every test out of
`#[cfg(test)] mod tests` (rejected: too large and risky a mechanical
migration to do blind). Instead: a crate opts a test in with
`#[ralphus_bench(patience = N)]` (see `ralphus-bench-macros`), which expands
to the original function, a separately-named `#[test]`-tagged wrapper that
calls it once (so it keeps running under plain `cargo test` too), and a
`const BenchMeta` describing it. The crate then hand-maintains a
`pub fn ralphus_bench_tests() -> Vec<BenchMeta>` collector (see
`core/src/bench_demo.rs` for the reference example) that `ralphus-bench-rs`
calls into. Adding a crate's tests to the harness is therefore a deliberate,
visible, per-test action.

**Authoring rule — patience.** Every time you add a benchmarked test
(`#[ralphus_bench(...)]`), deliberately consider whether the default patience
(10) is appropriate for that test. If you explicitly set a patience value —
even if it happens to equal the default — the line must carry a trailing
inline comment explaining why, e.g.:

```rust
#[ralphus_bench(patience = 3)] // low: validating an empty file is trivial and deterministic — few improving runs needed to find its floor
fn validate_toml_rejects_empty_file() { ... }
```

Tests using the implicit default (no `#[ralphus_bench]` argument at all) need
no comment.

## Bench Patience Comment Rule (RAL-94 / RAL-95)

The authoring rule above has two halves:

1. **Deliberately consider patience** whenever you add or touch a test.
   Don't default to leaving it unset out of habit — decide whether the test
   needs a non-default patience budget.
2. **Comment whenever a value is explicitly set.** If you write
   `patience = ...`, that line must carry a justification comment (`//`)
   explaining *why* that budget is needed (e.g. spins up a subprocess, hits
   a real Ollama model, rebuilds a worktree) — either as a trailing comment
   on the same line, or as a comment on the line immediately above.

Only the second half is mechanically checkable — "did you think about it" is
a review-time discipline, not something a script can verify. That's what
`scripts/check_bench_patience_comments.py` enforces: it scans every tracked
`*.rs` file for `#[ralphus_bench(patience = ...)]` (and, harmlessly, every
tracked `*.py` file for the now-retired Python equivalent — no `*.py` file
carries that pattern anymore since the Python side of RAL-94 was reduced to
`cli/src/ralphus/bench/`'s graph renderer, which never times any test), and
fails if a match lacks a same-line trailing comment or a comment on the line
above. It's stdlib-only (no `uv sync` needed) and runs as its own CI job
(`bench-patience`) on every PR, from the repo root:

```bash
python scripts/check_bench_patience_comments.py
```

A clean run prints nothing and exits 0; violations print one
`<path>:<line>: patience value set without an inline comment explaining why`
line per offending match and exit nonzero.
