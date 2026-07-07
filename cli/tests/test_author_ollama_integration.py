"""Ollama-backed smoke test for the agentic authoring generator.

Proves the real pydantic-ai -> Ollama path produces Task-TOML-shaped output for a
simple goal (and validates it offline when a ralphus-daemon binary is around).
Skipped when pydantic-ai is absent or Ollama is unreachable; fails only if Ollama
is up but the model is missing — matching ``test_ollama_integration.py``.

Run it explicitly with the runner extra:
    uv run --extra runner pytest -k author_ollama
"""

from __future__ import annotations

import json
import os
import shutil
import subprocess
import urllib.error
import urllib.request
from pathlib import Path

import pytest

pytest.importorskip("pydantic_ai", reason="pydantic-ai (runner extra) not installed")

from ralphus.author import build_system_prompt, build_user_prompt
from ralphus.author.core import Budget, VerifyIntent, _split_documents

OLLAMA_URL = os.environ.get("RALPHUS_OLLAMA_URL", "http://localhost:11434/v1")
OLLAMA_MODEL = os.environ.get("RALPHUS_OLLAMA_MODEL", "qwen3:8b")


def _ollama_models() -> list[str] | None:
    tags_url = OLLAMA_URL.rstrip("/").removesuffix("/v1") + "/api/tags"
    try:
        with urllib.request.urlopen(tags_url, timeout=3) as resp:
            data = json.loads(resp.read())
    except (urllib.error.URLError, TimeoutError, ValueError, OSError):
        return None
    return [m.get("name", "") for m in data.get("models", [])]


def _validate_offline(doc: str, tmp_path: Path) -> bool | None:
    """Validate a doc via the daemon binary, or None if no binary is available."""
    daemon_bin = os.environ.get("RALPHUS_DAEMON_BIN") or shutil.which("ralphus-daemon")
    if not daemon_bin:
        return None
    path = tmp_path / "candidate.toml"
    path.write_text(doc, encoding="utf-8")
    proc = subprocess.run([daemon_bin, "validate", str(path)], check=False)
    return proc.returncode == 0


def test_ollama_generator_authors_task_toml(tmp_path: Path) -> None:
    models = _ollama_models()
    if models is None:
        pytest.skip("Ollama is not running")
    if not any(m.startswith(OLLAMA_MODEL.split(":")[0]) for m in models):
        pytest.fail(f"Ollama is up but model {OLLAMA_MODEL!r} is not installed; models: {models}")

    from ralphus.author.agent import load_generator

    generator = load_generator("ollama", OLLAMA_MODEL)
    system_prompt = build_system_prompt(VerifyIntent(), wants_review=False)
    goal = (
        "Create one task named 'hello' with a single session whose cwd is "
        "'C:/tmp/demo' and whose prompt creates a file hello.txt containing 'hi'."
    )
    result = generator.generate(
        system_prompt, build_user_prompt(goal, None), budget=Budget(max_seconds=180.0)
    )

    docs = [doc for raw in result.tomls for doc in _split_documents(raw)]
    assert docs, "the agent produced no TOML"
    combined = "\n".join(docs)
    assert "[[task" in combined, f"output is not Task TOML shaped:\n{combined}"

    verdict = _validate_offline(docs[0], tmp_path)
    if verdict is not None:
        # Not a hard requirement (small local models are imperfect authors), but
        # surface the result so a passing run is visible in -s output.
        print(f"offline validation of first doc: {'PASS' if verdict else 'FAIL'}")
