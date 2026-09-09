# cli-py/ (Python project)

The old Python CLI/runner (and its whole test suite) is gone — this folder
now holds only `docsgen/` (doc screenshot generation, dev-only, never
shipped) and a trimmed `bench/` (renders the Rust bench harness's SVG/HTML
graphs from data `bench-harness/` writes — see
[[../bench-harness/AGENTS|bench-harness/AGENTS.md]] for the full RAL-94
design). CLI/runner work belongs in `cli/`/`runner/` and their own Rust
test suites, not here.

```bash
uv sync --dev
uv run ruff check .
uv run ruff format --check .
uv run mypy                         # strict; covers src + tests
uv run privata src                  # module-privacy linter for production code
uv run privata tests                # module-privacy linter for pytest support modules
uv run deadcode src tests           # unreferenced code linter; configured in cli-py/pyproject.toml
uv run pytest                       # run one: uv run pytest -k name
```

## Testing — Python tests

`cd cli-py && uv run pytest`. Key test files:

| File | Covers |
|---|---|
| `test_docsgen_helpmap.py` | `helpmap_docs.py`'s marker splice + drift-check logic, and a real (non-mocked) call into the compiled `ralphus show help-map` |
| `test_docsgen_lint.py` | `lint.py`'s `board_tabs()` scan: the `TABS` array found in a `board/*.js` chunk, in board.html itself, or nowhere (error) |
| `test_bench_storage.py` | Bench record read/write, filename hashing |
| `test_bench_graphs.py` | SVG/HTML rendering from stored records |
| `test_bench_gitinfo.py` | Git commit/dirty-state detection |
| `test_bench_stats.py` | Stats-bundle computation (mean/median/stddev/IQR/outliers) |

None of these need live external services — that's a hard requirement (see
the Ollama-test authoring rule in [[../daemon/AGENTS|daemon/AGENTS.md]], which
this folder is exempt from having at all since it has no daemon/model code).

## docsgen/

`cli-py/src/ralphus/docsgen/` — `shots.py` (Playwright scenario driver),
`stub_server.py` (deterministic `/api/*` JSON stub), `librarian_server.py`
(spawns the real, compiled `ralphus-librarian` binary against that stub),
`fixtures.py` (canned response data), `lint.py` (screenshot-coverage check),
`helpmap_docs.py` (regenerates `docs/cli-reference.md`'s help-map block from
`ralphus show help-map`), `binaries.py` (locates the compiled
`ralphus`/`ralphus-librarian` exes).

## bench/

`cli-py/src/ralphus/bench/` — renders the Rust bench harness's stored timing
data as SVG/HTML graphs: `storage.py` (record read/write), `gitinfo.py`,
`stats.py`, `graphs.py`. Does not run or time anything itself — see
[[../bench-harness/AGENTS|bench-harness/AGENTS.md]].
