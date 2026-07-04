"""Tests for the Claude Code CLI backend (command construction, no real CLI)."""

from __future__ import annotations

import shutil
import subprocess
from pathlib import Path
from typing import Any

import pytest

from ralphus.runner.backend import BackendError
from ralphus.runner.claude_code_backend import ClaudeCodeBackend
from ralphus.runner.tools import Workspace


class _Proc:
    def __init__(self, returncode: int, stdout: str = "", stderr: str = "") -> None:
        self.returncode = returncode
        self.stdout = stdout
        self.stderr = stderr


def test_builds_headless_subscription_command(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    ws = Workspace.create(str(tmp_path))
    monkeypatch.setattr(shutil, "which", lambda _program: None)  # keep the program name as-is
    captured: dict[str, Any] = {}

    def fake_run(cmd: list[str], **kwargs: Any) -> _Proc:
        captured["cmd"] = cmd
        captured["cwd"] = kwargs.get("cwd")
        return _Proc(0, stdout="did the thing")

    monkeypatch.setattr(subprocess, "run", fake_run)
    outcome = ClaudeCodeBackend().run("make a file", ws, model="sonnet")

    assert outcome.summary == "did the thing"
    cmd = captured["cmd"]
    assert cmd[:3] == ["claude", "-p", "make a file"]
    assert "--dangerously-skip-permissions" in cmd
    assert cmd[cmd.index("--model") + 1] == "sonnet"
    assert captured["cwd"] == ws.root


def test_program_is_overridable(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    ws = Workspace.create(str(tmp_path))
    monkeypatch.setenv("RALPHUS_CLAUDE_CMD", "my-claude")
    monkeypatch.setattr(shutil, "which", lambda _program: None)
    captured: dict[str, Any] = {}

    def fake_run(cmd: list[str], **_kwargs: Any) -> _Proc:
        captured["cmd"] = cmd
        return _Proc(0)

    monkeypatch.setattr(subprocess, "run", fake_run)
    ClaudeCodeBackend().run("x", ws, model=None)
    assert captured["cmd"][0] == "my-claude"


def test_nonzero_exit_raises(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    ws = Workspace.create(str(tmp_path))

    def fake_run(_cmd: list[str], **_kwargs: Any) -> _Proc:
        return _Proc(2, stderr="claude failed")

    monkeypatch.setattr(subprocess, "run", fake_run)
    with pytest.raises(BackendError, match="exited 2"):
        ClaudeCodeBackend().run("x", ws, model=None)


def test_routing_selects_claude_code() -> None:
    from ralphus.runner.__main__ import _load_backend

    backend = _load_backend("claude-code", [])
    assert isinstance(backend, ClaudeCodeBackend)
