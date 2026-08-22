# ralphus (cli/)

The `ralphus` CLI and runner are now Rust (`cli-rs/`, `runner/` in the repo
root) — see the repository's `AGENTS.md`. This `cli/` directory is a Python
project kept only for:

- `docsgen/` — generates the screenshots embedded in the docs site
  (`docs/site/pages/screenshots/`), driven by Playwright against the real,
  compiled `ralphus-librarian` binary.
- `bench/` — renders the RAL-94 benchmark harness's stored timing data
  (`bench_data/`, written by the Rust `ralphus-bench-rs` harness) as SVG/HTML
  graphs; it no longer runs or times any test itself.

See the [repository README](../README.md) for context.

## Development

```bash
uv sync --dev
uv run ruff check .
uv run ruff format --check .
uv run mypy
uv run privata src
uv run privata tests
uv run deadcode src tests
uv run pytest
```
