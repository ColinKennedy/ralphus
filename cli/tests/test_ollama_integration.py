"""Ollama-backed integration test for the native pydantic-ai runner.

This is the headline "real run through the system with a local model" test. It
is skipped when pydantic-ai is not installed or Ollama is not reachable, and it
errors only on a genuine failure of the pipeline (not on environment absence).

Run it explicitly with the runner extra:
    uv run --extra runner pytest -k ollama
"""

from __future__ import annotations

import json
import os
import urllib.error
import urllib.request
from pathlib import Path

import pytest

pytest.importorskip("pydantic_ai", reason="pydantic-ai (runner extra) not installed")

pytestmark = pytest.mark.ollama  # calls a live LLM; excluded from `--ralphus-bench` (RAL-94)

OLLAMA_URL = os.environ.get("RALPHUS_OLLAMA_URL", "http://localhost:11434/v1")
OLLAMA_MODEL = os.environ.get("RALPHUS_OLLAMA_MODEL", "qwen3:8b")


def _ollama_models() -> list[str] | None:
    """Return installed Ollama model names, or None if the server is unreachable."""
    tags_url = OLLAMA_URL.rstrip("/").removesuffix("/v1") + "/api/tags"
    try:
        with urllib.request.urlopen(tags_url, timeout=3) as resp:
            data = json.loads(resp.read())
    except (urllib.error.URLError, TimeoutError, ValueError, OSError):
        return None
    return [m.get("name", "") for m in data.get("models", [])]


def test_ollama_prompt_session_writes_a_file(tmp_path: Path) -> None:
    models = _ollama_models()
    if models is None:
        pytest.skip("Ollama is not running")
    if not any(m.startswith(OLLAMA_MODEL.split(":")[0]) for m in models):
        pytest.fail(f"Ollama is up but model {OLLAMA_MODEL!r} is not installed; models: {models}")

    from ralphus.runner.execute import run_session
    from ralphus.runner.spec import SessionSpec

    spec = SessionSpec.from_json(
        json.dumps(
            {
                "run_id": "run-ollama",
                "task": "write",
                "session_id": "s0",
                "cwd": str(tmp_path),
                "agent": "ollama",
                "model": OLLAMA_MODEL,
                "prompt": (
                    "Create a file named result.txt whose exact content is the single "
                    "line: RALPHUS_LOCAL_OK"
                ),
            }
        )
    )

    from ralphus.runner.pydantic_backend import load_backend

    result = run_session(spec, backend=load_backend("ollama"))

    assert result.ok, f"session failed: {result.error}"
    produced = tmp_path / "result.txt"
    assert produced.exists(), "the agent did not create result.txt"
    assert "RALPHUS_LOCAL_OK" in produced.read_text(encoding="utf-8")
