# Benchmark Harness (RAL-94)

A custom, opt-in harness that durably times every test in the repo, one commit
at a time, so per-test timing regressions are visible per commit instead of
only "the whole suite got slow" after the fact. It never runs as part of
normal `pytest` / `cargo test` — it is a separate, explicit invocation, split
across two independent steps: generating data, and rendering graphs from it.

## Step 1 — Generate data (runs tests, writes JSON)

Per-language; each writes into its own subtree of `bench_data/` so the two
ecosystems' data is never intermixed.

```bash
# Python: benchmarks every collected pytest test
cd cli && uv run ralphus-bench-py

# Rust: benchmarks every #[ralphus_bench]-tagged test
cargo run -p ralphus-bench-harness --bin ralphus-bench-rs
```

Both implement the same "durable minimum" stopping rule: run the test once,
in-process; if it beat the best duration seen so far, remember it and reset
`patience`; otherwise decrement `patience`; stop at `patience == 0`. Every raw
sample is kept and reduced to a full stats bundle (`max`, `mean`, `median`,
`stddev`, `iqr`, Tukey `outliers`) alongside `durable_min`. Results accumulate
one record per commit per test — see `cli/src/ralphus/bench/storage.py` and
`bench-harness/src/storage.rs`.

A skipped Python test (pytest `skip`/`skipif`, or a mid-test `pytest.skip()`)
is recorded explicitly as a `skipped` entry alongside `records`, rather than
simply being absent, so a skip is distinguishable from "this test didn't
exist yet." Graph rendering only ever reads `.records`, so a skip is surfaced
as a count and never plotted as a trend-line point. Rust has no runtime
equivalent — a crate's hand-maintained `ralphus_bench_tests()` collector
simply omits a test it doesn't want benchmarked, which is already an
intentional per-test opt-in decision.

### Tests that call a live LLM are excluded outright

A test that calls out to a real Ollama model (`test_ollama_integration.py`,
`test_verify_ollama_integration.py`, `test_author_ollama_integration.py`) is
tagged with `pytestmark = pytest.mark.ollama`. `pytest_collection_modifyitems`
in `cli/src/ralphus/bench/pytest_plugin.py` deselects every `ollama`-marked
test whenever `--ralphus-bench` is active, before any test runs — not run
once untimed, not recorded as `skipped`, just excluded from the harness run
entirely. A live model call's duration reflects inference/network latency,
not this repo's own performance, so it isn't meaningful timing-regression
data — and repeatedly re-invoking one under the durable-minimum loop would be
slow and expensive for no benefit.

This only affects `--ralphus-bench` runs. A plain `pytest` invocation is
unaffected — the `ollama` marker is inert without the flag, so these tests
still run (or self-skip when Ollama/pydantic-ai is unavailable) exactly as
before. To mark a new test the same way, either add
`pytestmark = pytest.mark.ollama` at module level (if the whole file calls a
live model) or `@pytest.mark.ollama` on the individual test function.

### The general escape hatch: `no_bench`

`run_durable_min` calls a test's body **at least twice** in-process, always
— the first call sets the initial best duration, so `patience` only controls
how many further non-improving calls it takes to stop; there's no patience
value that makes it call a test exactly once. That's invisible to a pure
computation, but it breaks two kinds of test:

- **Real network/socket calls.** A test hitting a refused connection with a
  real client timeout gets that timeout paid on every one of those repeated
  calls. One isolated case (`test_check_health_runs` in `test_cli.py`)
  measured at ~4s/call, ballooning to 75s total for what should be an
  instant test — its duration reflects network/OS latency, not this repo's
  performance, so it isn't meaningful regression data anyway. `test_health.py`
  is the same story for every test in the file (each calls `run_checks()`,
  which always probes a real daemon and Ollama socket).
- **State that persists across the repeated calls.** A fixture resolved once
  per test item (`tmp_path`, `pytester`) is the *same* value across all of a
  benched test's repeated invocations. A test whose body appends to a file
  and then asserts an exact count, does a one-shot `mkdir()`, or commits to a
  git repo expecting something to have changed, breaks on the second call.

Where the fix is cheap, prefer making the test tolerant of repeat invocation
instead of excluding it:
- Swap a fixed `tmp_path` for a fresh `tmp_path_factory.mktemp(...)` call
  *inside* the test body — since that's a real function call (not a fixture
  resolved once), it returns a genuinely new directory on every invocation.
  See the fixes to `test_bench_storage.py` for the pattern.
- Make one-shot setup idempotent, e.g. an early-return in a `_git_init`
  helper if `.git` already exists (`test_bench_pytest_plugin.py`).
- Add `exist_ok=True` to a `mkdir()` if the test doesn't assert an exact
  count that repeat invocation would inflate anyway
  (`test_bench_graphs.py`).

Where it isn't cheap — e.g. a `pytester` sandbox's directory can't easily be
swapped out for a fresh one from inside the test body, so an integration
test that asserts an exact accumulated record count has no easy fix — tag it
`pytestmark = pytest.mark.no_bench` (module level) or `@pytest.mark.no_bench`
(single test), deselected by the same collection hook as `ollama`. Use
`no_bench` for this general "not safe or meaningful to invoke repeatedly"
reasoning; reserve `ollama` for the specific "calls a live LLM" case. Always
give the marker a trailing/leading comment explaining *why* — the marker
itself doesn't say whether the reason was a socket timeout or accumulated
state.

## Step 2 — Render graphs (reads already-stored data, no test execution)

One shared command for both languages, run separately from — and after —
step 1:

```bash
cd cli && uv run ralphus-bench-graph --lang all   # or --lang python / --lang rust
```

This step never executes a test; it only reads whatever JSON is already on
disk under `bench_data/`. Running a data-generation command from step 1 does
**not** trigger this step automatically — re-run it any time you want the
graphs to reflect newly recorded data.

It writes a standalone HTML page a developer opens directly —
`bench_data/index.html` — not raw `.svg` files, and not part of `board.html`
or the public docs site. That page links to a per-language landing page
(`bench_data/<language>/index.html`), which links to a per-file/module page
(`bench_data/<language>/.../index.html`) that embeds that group's summary,
multiline, and per-test SVGs via `<img>`. Everything is plain string-built
SVG (`cli/src/ralphus/bench/graphs.py`) — no charting library, no new
dependency.

Three graph types per file/module group:

| Graph | Contents |
|---|---|
| Per-test | One line, one test, x = commit, y = `durable_min` |
| Per-file multiline | Every test in the group on one plot; each series labeled with a letter drawn on the plot (color is hashed from the test's identity, since cardinality is unbounded), legend below maps letters to test names |
| Per-file summary | min / median / mean / max of `durable_min` aggregated across the group's tests per commit; fixed 4-series categorical palette |

## Why Rust needs explicit per-test opt-in but Python doesn't

Python achieves true universal inclusion — `ralphus.bench.pytest_plugin`
hooks `pytest_pyfunc_call` and takes over every collected test's invocation
the moment `--ralphus-bench` is passed, so no per-test annotation is needed.

Rust falls back to explicit per-test opt-in, per this ticket's own documented
escape hatch — not laziness. `cargo test`'s implicit lib-unittest target (the
one that runs `#[cfg(test)] mod tests` inline in `src/`) executes the vast
majority of this repo's Rust tests, and Cargo gives no way to swap out or
disable that target's harness — `harness = false` only applies to a
`[[test]]` entry pointing at a file under `tests/`, not to the implicit
inline-unittest target. There is therefore no way for an external
`ralphus-bench-rs` binary to reach into an ordinary `#[test]` function and
re-invoke it in-process, repeatedly, without either linker-level magic
(rejected: this workspace forbids `unsafe_code`) or moving every test out of
`#[cfg(test)] mod tests` (rejected: too large and risky a mechanical
migration to do blind).

Instead: a crate opts a test in with `#[ralphus_bench(patience = N)]` (see
`ralphus-bench-macros`), which expands to the original function, a
separately-named `#[test]`-tagged wrapper that calls it once (so it keeps
running under plain `cargo test` too), and a `const BenchMeta` describing it.
The crate then hand-maintains a `pub fn ralphus_bench_tests() -> Vec<BenchMeta>`
collector (see `core/src/bench_demo.rs` for the reference example) that
`ralphus-bench-rs` calls into. Adding a crate's tests to the Rust harness is
therefore a deliberate, visible, per-test action.

## Authoring rule — patience

Every time you add a benchmarked test (Python
`@pytest.mark.ralphus_bench(...)` or Rust `#[ralphus_bench(...)]`),
deliberately consider whether the default patience (10) is appropriate. If
you explicitly set a patience value — even if it happens to equal the
default — the line must carry a trailing inline comment explaining why:

```python
@pytest.mark.ralphus_bench(patience=1)  # low: hits Ollama, each call is slow/expensive — bail after one non-improving run
def test_ollama_backed_thing(): ...
```

```rust
#[ralphus_bench(patience = 3)] // low: validating an empty file is trivial and deterministic — few improving runs needed to find its floor
fn validate_toml_rejects_empty_file() { ... }
```

Tests using the implicit default (no `patience=` argument, or no
`#[ralphus_bench]` argument at all) need no comment.

## Where filenames come from

Test-name-derived filename stems are hashed (not embedded verbatim) once they
run long, since Windows' `MAX_PATH` is 260 chars and parametrized pytest
names can blow past a safe budget on their own — see
`_short_identifier` in `cli/src/ralphus/bench/storage.py`.
