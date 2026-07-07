"""Tests for the Claude Code CLI backend (command construction, no real CLI)."""

from __future__ import annotations

import shutil
import subprocess
from pathlib import Path
from typing import Any

import pytest

from ralphus.runner.backend import BackendError
from ralphus.runner.claude_code_backend import ClaudeCodeBackend, _write_prompt_file
from ralphus.runner.tools import Workspace


class _Proc:
    def __init__(self, returncode: int, stdout: str = "", stderr: str = "") -> None:
        self.returncode = returncode
        self.stdout = stdout
        self.stderr = stderr


def _prompt_from_cmd(cmd: list[str]) -> str:
    """Read the prompt file referenced by the -p @<path> argument."""
    p_arg = cmd[cmd.index("-p") + 1]
    assert p_arg.startswith("@"), f"expected @<path>, got {p_arg!r}"
    return Path(p_arg[1:]).read_text(encoding="utf-8")


def test_builds_headless_subscription_command(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    ws = Workspace.create(str(tmp_path))
    monkeypatch.setattr(shutil, "which", lambda _program: None)  # keep the program name as-is
    captured: dict[str, Any] = {}

    def fake_run(cmd: list[str], **kwargs: Any) -> _Proc:
        captured["cmd"] = cmd
        captured["cwd"] = kwargs.get("cwd")
        captured["prompt"] = _prompt_from_cmd(cmd)
        return _Proc(0, stdout="did the thing")

    monkeypatch.setattr(subprocess, "run", fake_run)
    outcome = ClaudeCodeBackend().run("make a file", ws, model="sonnet")

    assert outcome.summary == "did the thing"
    cmd = captured["cmd"]
    assert cmd[1] == "-p"
    assert cmd[2].startswith("@")
    assert captured["prompt"] == "make a file"
    assert "--dangerously-skip-permissions" in cmd
    assert cmd[cmd.index("--model") + 1] == "sonnet"
    assert captured["cwd"] == ws.root


def test_prompt_file_is_removed_after_run(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    ws = Workspace.create(str(tmp_path))
    monkeypatch.setattr(shutil, "which", lambda _program: None)
    recorded_path: list[Path] = []

    def fake_run(cmd: list[str], **_kwargs: Any) -> _Proc:
        p_arg = cmd[cmd.index("-p") + 1]
        recorded_path.append(Path(p_arg[1:]))
        return _Proc(0)

    monkeypatch.setattr(subprocess, "run", fake_run)
    ClaudeCodeBackend().run("hello", ws, model=None)

    assert recorded_path, "fake_run was never called"
    assert not recorded_path[0].exists(), "prompt file was not cleaned up"


def test_prompt_file_is_removed_even_on_error(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    ws = Workspace.create(str(tmp_path))
    monkeypatch.setattr(shutil, "which", lambda _program: None)
    recorded_path: list[Path] = []

    def fake_run(cmd: list[str], **_kwargs: Any) -> _Proc:
        p_arg = cmd[cmd.index("-p") + 1]
        recorded_path.append(Path(p_arg[1:]))
        raise OSError("simulated failure")

    monkeypatch.setattr(subprocess, "run", fake_run)
    with pytest.raises(BackendError):
        ClaudeCodeBackend().run("hello", ws, model=None)

    assert recorded_path, "fake_run was never called"
    assert not recorded_path[0].exists(), "prompt file was not cleaned up after error"


def test_append_system_prompt_maps_to_cli_flag(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    ws = Workspace.create(str(tmp_path))
    monkeypatch.setattr(shutil, "which", lambda _program: None)
    captured: dict[str, Any] = {}

    def fake_run(cmd: list[str], **_kwargs: Any) -> _Proc:
        captured["cmd"] = cmd
        return _Proc(0, stdout="ok")

    monkeypatch.setattr(subprocess, "run", fake_run)
    ClaudeCodeBackend().run(
        "do work", ws, model=None, append_system_prompt="Follow the house style."
    )

    cmd = captured["cmd"]
    assert cmd[cmd.index("--append-system-prompt") + 1] == "Follow the house style."
    # The system prompt must NOT be concatenated into the user prompt.
    assert cmd[cmd.index("-p") + 1].startswith("@")


def test_no_append_system_prompt_flag_when_unset(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    ws = Workspace.create(str(tmp_path))
    monkeypatch.setattr(shutil, "which", lambda _program: None)
    captured: dict[str, Any] = {}

    def fake_run(cmd: list[str], **_kwargs: Any) -> _Proc:
        captured["cmd"] = cmd
        return _Proc(0)

    monkeypatch.setattr(subprocess, "run", fake_run)
    ClaudeCodeBackend().run("x", ws, model=None)
    assert "--append-system-prompt" not in captured["cmd"]


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


def test_write_prompt_file_roundtrip(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    monkeypatch.setattr(Path, "home", lambda: tmp_path)
    path = _write_prompt_file("hello world")
    assert path.read_text(encoding="utf-8") == "hello world"
    path.unlink()


def test_write_prompt_file_same_content_same_path(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    monkeypatch.setattr(Path, "home", lambda: tmp_path)
    p1 = _write_prompt_file("same prompt")
    p2 = _write_prompt_file("same prompt")
    assert p1 == p2
    p1.unlink(missing_ok=True)


def test_write_prompt_file_different_content_different_path(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    monkeypatch.setattr(Path, "home", lambda: tmp_path)
    p1 = _write_prompt_file("prompt A")
    p2 = _write_prompt_file("prompt B")
    assert p1 != p2
    p1.unlink(missing_ok=True)
    p2.unlink(missing_ok=True)
