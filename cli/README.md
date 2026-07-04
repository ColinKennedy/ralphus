# ralphus (CLI)

The Python CLI and agent runner for ralphus. A thin client over the daemon's
HTTP/JSON API, plus the pydantic-ai-based runner that executes agent sessions.

See the [repository README](../README.md) and `../PLAN.local.md` for context.

## Development

```bash
uv sync --dev
uv run ruff check .
uv run ruff format --check .
uv run mypy
uvx privata src
uv run pytest
```
