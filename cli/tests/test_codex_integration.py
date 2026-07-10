"""Codex CLI integration test.

Runs a real session against the Codex CLI (``npm install -g @openai/codex``).
Skipped automatically when ``codex`` is not on PATH (and ``RALPHUS_CODEX_CMD``
is not set) or ``OPENAI_API_KEY`` is absent.

Run it explicitly:
    uv run pytest -k codex_integration
"""

from __future__ import annotations

import json
import os
import shutil
from pathlib import Path

import pytest


def _codex_available() -> bool:
    """Return True if a codex executable can be resolved."""
    cmd = os.environ.get("RALPHUS_CODEX_CMD", "codex")
    return shutil.which(cmd) is not None


def _skip_unless_runnable() -> None:
    if not _codex_available():
        pytest.skip("codex not found on PATH (set RALPHUS_CODEX_CMD or install @openai/codex)")
    if not os.environ.get("OPENAI_API_KEY"):
        pytest.skip("OPENAI_API_KEY is not set")


def test_codex_prompt_session_writes_a_file(tmp_path: Path) -> None:
    _skip_unless_runnable()

    from ralphus.runner.execute import run_session
    from ralphus.runner.spec import SessionSpec

    spec = SessionSpec.from_json(
        json.dumps(
            {
                "run_id": "run-codex",
                "task": "write",
                "session_id": "s0",
                "cwd": str(tmp_path),
                "agent": "codex",
                "prompt": (
                    "Create a file named result.txt whose exact content is the single "
                    "line: RALPHUS_CODEX_OK"
                ),
            }
        )
    )

    from ralphus.runner.codex_backend import CodexBackend

    result = run_session(spec, backend=CodexBackend())

    assert result.ok, f"session failed: {result.error}"
    produced = tmp_path / "result.txt"
    assert produced.exists(), "the agent did not create result.txt"
    assert "RALPHUS_CODEX_OK" in produced.read_text(encoding="utf-8")
