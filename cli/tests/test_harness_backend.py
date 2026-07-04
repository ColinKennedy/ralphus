"""Tests for the harness-subprocess backend, using the Python interpreter as a
stand-in external harness."""

from __future__ import annotations

import sys
from pathlib import Path

import pytest

from ralphus.runner.backend import BackendError
from ralphus.runner.harness_backend import HarnessBackend
from ralphus.runner.tools import Workspace

# A tiny "harness" that writes its final argument (the prompt) to a file.
_WRITER = "import sys, pathlib; pathlib.Path('harness_out.txt').write_text(sys.argv[-1])"
# A "harness" that always fails.
_FAILER = "import sys; sys.exit(3)"


def test_harness_runs_program_in_workspace(tmp_path: Path) -> None:
    ws = Workspace.create(str(tmp_path))
    backend = HarnessBackend(sys.executable, ["-c", _WRITER])
    outcome = backend.run("HELLO_HARNESS", ws, model=None)
    assert isinstance(outcome.summary, str)
    assert (tmp_path / "harness_out.txt").read_text(encoding="utf-8") == "HELLO_HARNESS"


def test_harness_nonzero_exit_raises(tmp_path: Path) -> None:
    ws = Workspace.create(str(tmp_path))
    backend = HarnessBackend(sys.executable, ["-c", _FAILER])
    with pytest.raises(BackendError, match="exited 3"):
        backend.run("anything", ws, model=None)


def test_missing_harness_raises(tmp_path: Path) -> None:
    ws = Workspace.create(str(tmp_path))
    backend = HarnessBackend("definitely-not-a-real-harness-xyz")
    with pytest.raises(BackendError, match="could not run harness"):
        backend.run("x", ws, model=None)


def test_harness_used_via_run_session(tmp_path: Path) -> None:
    # A prompt session whose agent is an external harness runs it end-to-end.
    import json

    from ralphus.runner.execute import run_session
    from ralphus.runner.harness_backend import HarnessBackend as HB
    from ralphus.runner.spec import SessionSpec

    spec = SessionSpec.from_json(
        json.dumps(
            {
                "run_id": "r",
                "task": "t",
                "session_id": "s",
                "cwd": str(tmp_path),
                "agent": "my-harness",
                "prompt": "WRITE_ME",
            }
        )
    )
    result = run_session(spec, backend=HB(sys.executable, ["-c", _WRITER]))
    assert result.ok, result.error
    assert (tmp_path / "harness_out.txt").read_text(encoding="utf-8") == "WRITE_ME"
