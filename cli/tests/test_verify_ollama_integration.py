"""Ollama-backed integration test for agent-kind verify execution.

This is the headline "a real AI verifier judges a real claim" test for the
`agent`-kind verify step: it drives `run_session` with `verify=True` against a
real local Ollama model (no mocking) and checks that a true claim is judged
PASS and a false one is judged FAIL. It is skipped when pydantic-ai is not
installed or Ollama is not reachable, and it errors only on a genuine failure
of the pipeline (not on environment absence) — same skip/fail split as
`test_ollama_integration.py`.

The claims are deliberately answerable without any tool call (plain
arithmetic) so this test exercises the verify-wrapping/verdict-parsing
pipeline itself, rather than a small local model's tool-calling reliability —
a separate, already-known limitation that isn't specific to this feature (see
e.g. `ralphus author`'s own live-ollama flakiness).

Deselected by default (see `addopts` in cli/pyproject.toml). Run it explicitly
with the runner extra:
    uv run --extra runner pytest -m ollama -k verify_ollama
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


def _require_ollama() -> None:
    models = _ollama_models()
    if models is None:
        pytest.skip("Ollama is not running")
    if not any(m.startswith(OLLAMA_MODEL.split(":")[0]) for m in models):
        pytest.fail(f"Ollama is up but model {OLLAMA_MODEL!r} is not installed; models: {models}")


def _verify_spec_json(tmp_path: Path, session_id: str, claim: str) -> str:
    return json.dumps(
        {
            "run_id": "run-ollama-verify",
            "task": "verify",
            "session_id": session_id,
            "cwd": str(tmp_path),
            "agent": "ollama",
            "model": OLLAMA_MODEL,
            "verify": True,
            "prompt": f"Confirm this arithmetic claim: {claim}",
        }
    )


def test_ollama_agent_verify_passes_a_true_claim(tmp_path: Path) -> None:
    _require_ollama()

    from ralphus.runner.execute import run_session
    from ralphus.runner.pydantic_backend import load_backend
    from ralphus.runner.spec import SessionSpec

    spec = SessionSpec.from_json(_verify_spec_json(tmp_path, "v0", "2 + 2 = 4"))
    result = run_session(spec, backend=load_backend("ollama"))

    assert result.ok, f"verifier crashed: {result.error}"
    assert result.verified is True, f"expected a PASS verdict, got: {result.summary!r}"


def test_ollama_agent_verify_fails_a_false_claim(tmp_path: Path) -> None:
    _require_ollama()

    from ralphus.runner.execute import run_session
    from ralphus.runner.pydantic_backend import load_backend
    from ralphus.runner.spec import SessionSpec

    spec = SessionSpec.from_json(_verify_spec_json(tmp_path, "v1", "2 + 2 = 5"))
    result = run_session(spec, backend=load_backend("ollama"))

    assert result.ok, f"verifier crashed: {result.error}"
    assert result.verified is False, f"expected a FAIL verdict, got: {result.summary!r}"
