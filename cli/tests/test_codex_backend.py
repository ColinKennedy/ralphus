"""Tests for the Codex CLI backend (command construction, no real CLI)."""

from __future__ import annotations

import shutil
import subprocess
from pathlib import Path
from typing import Any

import pytest

from ralphus.runner.backend import BackendError
from ralphus.runner.codex_backend import CodexBackend, _write_prompt_file
from ralphus.runner.tools import Workspace


class _Proc:
    def __init__(self, returncode: int, stdout: str = "", stderr: str = "") -> None:
        self.returncode = returncode
        self.stdout = stdout
        self.stderr = stderr


def test_builds_exec_command(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    ws = Workspace.create(str(tmp_path))
    monkeypatch.setattr(shutil, "which", lambda _program: None)
    captured: dict[str, Any] = {}

    def fake_run(cmd: list[str], **kwargs: Any) -> _Proc:
        captured["cmd"] = cmd
        captured["cwd"] = kwargs.get("cwd")
        captured["input"] = kwargs.get("input", "")
        return _Proc(0, stdout="task done")

    monkeypatch.setattr(subprocess, "run", fake_run)
    outcome = CodexBackend().run("do the thing", ws, model="o4-mini")

    assert outcome.summary == "task done"
    cmd = captured["cmd"]
    assert cmd[0] == "codex"
    assert cmd[1] == "exec"
    assert "--dangerously-bypass-approvals-and-sandbox" in cmd
    assert "--skip-git-repo-check" in cmd
    assert "--ephemeral" in cmd
    assert "-C" in cmd
    assert cmd[cmd.index("-C") + 1] == str(ws.root)
    assert cmd[-1] == "-"
    assert cmd[cmd.index("-m") + 1] == "o4-mini"
    assert captured["cwd"] == ws.root


def test_prompt_is_passed_via_stdin(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    ws = Workspace.create(str(tmp_path))
    monkeypatch.setattr(shutil, "which", lambda _program: None)
    captured: dict[str, Any] = {}

    def fake_run(cmd: list[str], **kwargs: Any) -> _Proc:
        captured["input"] = kwargs.get("input", "")
        # Verify the prompt is NOT on the command line as a plain argument.
        captured["cmd"] = cmd
        return _Proc(0)

    monkeypatch.setattr(subprocess, "run", fake_run)
    CodexBackend().run("my prompt text", ws, model=None)

    assert captured["input"] == "my prompt text"
    # The prompt itself should not appear as a command-line argument.
    assert "my prompt text" not in captured["cmd"]


def test_prompt_file_is_removed_after_run(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    ws = Workspace.create(str(tmp_path))
    monkeypatch.setattr(shutil, "which", lambda _program: None)
    monkeypatch.setattr(Path, "home", lambda: tmp_path)

    def fake_run(_cmd: list[str], **_kwargs: Any) -> _Proc:
        return _Proc(0)

    monkeypatch.setattr(subprocess, "run", fake_run)
    CodexBackend().run("hello", ws, model=None)

    leftover = list((tmp_path / ".ralphus" / "task_prompts").glob("*.md"))
    assert leftover == [], f"prompt file was not cleaned up: {leftover}"


def test_prompt_file_is_removed_even_on_error(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    ws = Workspace.create(str(tmp_path))
    monkeypatch.setattr(shutil, "which", lambda _program: None)
    monkeypatch.setattr(Path, "home", lambda: tmp_path)

    def fake_run(_cmd: list[str], **_kwargs: Any) -> _Proc:
        raise OSError("simulated failure")

    monkeypatch.setattr(subprocess, "run", fake_run)
    with pytest.raises(BackendError):
        CodexBackend().run("hello", ws, model=None)

    leftover = list((tmp_path / ".ralphus" / "task_prompts").glob("*.md"))
    assert leftover == [], f"prompt file was not cleaned up after error: {leftover}"


def test_no_model_flag_when_unset(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    ws = Workspace.create(str(tmp_path))
    monkeypatch.setattr(shutil, "which", lambda _program: None)
    captured: dict[str, Any] = {}

    def fake_run(cmd: list[str], **_kwargs: Any) -> _Proc:
        captured["cmd"] = cmd
        return _Proc(0)

    monkeypatch.setattr(subprocess, "run", fake_run)
    CodexBackend().run("x", ws, model=None)
    assert "-m" not in captured["cmd"]


def test_program_is_overridable(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    ws = Workspace.create(str(tmp_path))
    monkeypatch.setenv("RALPHUS_CODEX_CMD", "my-codex")
    monkeypatch.setattr(shutil, "which", lambda _program: None)
    captured: dict[str, Any] = {}

    def fake_run(cmd: list[str], **_kwargs: Any) -> _Proc:
        captured["cmd"] = cmd
        return _Proc(0)

    monkeypatch.setattr(subprocess, "run", fake_run)
    CodexBackend().run("x", ws, model=None)
    assert captured["cmd"][0] == "my-codex"


def test_nonzero_exit_raises(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    ws = Workspace.create(str(tmp_path))

    def fake_run(_cmd: list[str], **_kwargs: Any) -> _Proc:
        return _Proc(1, stderr="codex failed")

    monkeypatch.setattr(subprocess, "run", fake_run)
    with pytest.raises(BackendError, match="exited 1"):
        CodexBackend().run("x", ws, model=None)


def test_oserror_raises_backend_error(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    ws = Workspace.create(str(tmp_path))
    monkeypatch.setattr(shutil, "which", lambda _program: None)

    def fake_run(_cmd: list[str], **_kwargs: Any) -> _Proc:
        raise OSError("no such file")

    monkeypatch.setattr(subprocess, "run", fake_run)
    with pytest.raises(BackendError, match="could not run Codex"):
        CodexBackend().run("x", ws, model=None)


def test_routing_selects_codex() -> None:
    from ralphus.runner.__main__ import _load_backend

    backend = _load_backend("codex", [])
    assert isinstance(backend, CodexBackend)


def test_routing_selects_codex_cli() -> None:
    from ralphus.runner.__main__ import _load_backend

    backend = _load_backend("codex-cli", [])
    assert isinstance(backend, CodexBackend)


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
