"""Tests for the Claude Code CLI backend (command construction, no real CLI)."""

from __future__ import annotations

import io
import json
import shutil
import subprocess
import tempfile
from collections.abc import Iterator
from pathlib import Path
from typing import Any

import pytest

from ralphus.runner.backend import BackendError
from ralphus.runner.claude_code_backend import (
    ClaudeCodeBackend,
    _write_prompt_file,
    live_session_path,
)
from ralphus.runner.tools import Workspace


def _result_event(
    result: str = "",
    session_id: str | None = None,
    tokens_in: int = 0,
    tokens_out: int = 0,
    cost: float = 0.0,
) -> str:
    return json.dumps(
        {
            "type": "result",
            "subtype": "success",
            "result": result,
            "session_id": session_id,
            "total_input_tokens": tokens_in,
            "total_output_tokens": tokens_out,
            "total_cost_usd": cost,
        }
    )


class _FakePopen:
    """Minimal subprocess.Popen stand-in for stream-json output."""

    def __init__(
        self,
        returncode: int = 0,
        stdout_lines: list[str] | None = None,
        stderr_str: str = "",
    ) -> None:
        self.returncode = returncode
        self.stdout: Iterator[str] = iter([line + "\n" for line in (stdout_lines or [])])
        self.stderr = io.StringIO(stderr_str)

    def wait(self, timeout: float | None = None) -> int:
        return self.returncode


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

    def fake_popen(cmd: list[str], **kwargs: Any) -> _FakePopen:
        captured["cmd"] = cmd
        captured["cwd"] = kwargs.get("cwd")
        captured["prompt"] = _prompt_from_cmd(cmd)
        return _FakePopen(0, stdout_lines=[_result_event("did the thing")])

    monkeypatch.setattr(subprocess, "Popen", fake_popen)
    outcome = ClaudeCodeBackend().run("make a file", ws, model="sonnet")

    assert outcome.summary == "did the thing"
    cmd = captured["cmd"]
    assert cmd[1] == "-p"
    assert cmd[2].startswith("@")
    assert captured["prompt"] == "make a file"
    assert "--dangerously-skip-permissions" in cmd
    assert "--output-format" in cmd
    assert cmd[cmd.index("--output-format") + 1] == "stream-json"
    assert cmd[cmd.index("--model") + 1] == "sonnet"
    assert captured["cwd"] == ws.root


def test_prompt_file_is_removed_after_run(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    ws = Workspace.create(str(tmp_path))
    monkeypatch.setattr(shutil, "which", lambda _program: None)
    monkeypatch.delenv("RALPHUS_CONFIGURATION_PATH", raising=False)
    recorded_path: list[Path] = []

    def fake_popen(cmd: list[str], **_kwargs: Any) -> _FakePopen:
        p_arg = cmd[cmd.index("-p") + 1]
        recorded_path.append(Path(p_arg[1:]))
        return _FakePopen(0)

    monkeypatch.setattr(subprocess, "Popen", fake_popen)
    ClaudeCodeBackend().run("hello", ws, model=None)

    assert recorded_path, "fake_popen was never called"
    assert not recorded_path[0].exists(), "prompt file was not cleaned up"


def test_prompt_file_is_removed_even_on_error(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    ws = Workspace.create(str(tmp_path))
    monkeypatch.setattr(shutil, "which", lambda _program: None)
    monkeypatch.delenv("RALPHUS_CONFIGURATION_PATH", raising=False)
    recorded_path: list[Path] = []

    def fake_popen(cmd: list[str], **_kwargs: Any) -> _FakePopen:
        p_arg = cmd[cmd.index("-p") + 1]
        recorded_path.append(Path(p_arg[1:]))
        raise OSError("simulated failure")

    monkeypatch.setattr(subprocess, "Popen", fake_popen)
    with pytest.raises(BackendError):
        ClaudeCodeBackend().run("hello", ws, model=None)

    assert recorded_path, "fake_popen was never called"
    assert not recorded_path[0].exists(), "prompt file was not cleaned up after error"


def test_append_system_prompt_maps_to_cli_flag(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    ws = Workspace.create(str(tmp_path))
    monkeypatch.setattr(shutil, "which", lambda _program: None)
    captured: dict[str, Any] = {}

    def fake_popen(cmd: list[str], **_kwargs: Any) -> _FakePopen:
        captured["cmd"] = cmd
        return _FakePopen(0, stdout_lines=[_result_event("ok")])

    monkeypatch.setattr(subprocess, "Popen", fake_popen)
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

    def fake_popen(cmd: list[str], **_kwargs: Any) -> _FakePopen:
        captured["cmd"] = cmd
        return _FakePopen(0)

    monkeypatch.setattr(subprocess, "Popen", fake_popen)
    ClaudeCodeBackend().run("x", ws, model=None)
    assert "--append-system-prompt" not in captured["cmd"]


def test_program_is_overridable(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    ws = Workspace.create(str(tmp_path))
    monkeypatch.setenv("RALPHUS_CLAUDE_COMMAND", "my-claude")
    monkeypatch.setattr(shutil, "which", lambda _program: None)
    captured: dict[str, Any] = {}

    def fake_popen(cmd: list[str], **_kwargs: Any) -> _FakePopen:
        captured["cmd"] = cmd
        return _FakePopen(0)

    monkeypatch.setattr(subprocess, "Popen", fake_popen)
    ClaudeCodeBackend().run("x", ws, model=None)
    assert captured["cmd"][0] == "my-claude"


def test_nonzero_exit_raises(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    ws = Workspace.create(str(tmp_path))

    def fake_popen(_cmd: list[str], **_kwargs: Any) -> _FakePopen:
        return _FakePopen(2, stderr_str="claude failed")

    monkeypatch.setattr(subprocess, "Popen", fake_popen)
    with pytest.raises(BackendError, match="exited 2"):
        ClaudeCodeBackend().run("x", ws, model=None)


def test_session_id_written_to_temp_file(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    """Session ID is written to the temp-dir side-channel file on the init event."""
    ws = Workspace.create(str(tmp_path))
    monkeypatch.setattr(shutil, "which", lambda _program: None)
    written_sid: list[str] = []

    init_event = json.dumps({"type": "system", "subtype": "init", "session_id": "abc-123"})

    def fake_popen(cmd: list[str], **_kwargs: Any) -> _FakePopen:
        # Capture the sid_path and its content after the init event is processed.
        popen = _FakePopen(
            0, stdout_lines=[init_event, _result_event("done", session_id="abc-123")]
        )
        return popen

    captured_path: list[Path] = []

    original_write = Path.write_text

    def spy_write(self: Path, text: str, **kwargs: Any) -> None:
        if self.suffix == ".live_session":
            written_sid.append(text)
            captured_path.append(self)
        original_write(self, text, **kwargs)

    monkeypatch.setattr(Path, "write_text", spy_write)
    monkeypatch.setattr(subprocess, "Popen", fake_popen)
    outcome = ClaudeCodeBackend().run("task", ws, model=None)

    assert outcome.claude_session_id == "abc-123"
    assert "abc-123" in written_sid, "session ID was not written to live_session file"


def test_live_session_file_cleaned_up_after_run(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """The temp side-channel file is removed when the run finishes."""
    ws = Workspace.create(str(tmp_path))
    monkeypatch.setattr(shutil, "which", lambda _program: None)

    init_event = json.dumps({"type": "system", "subtype": "init", "session_id": "xyz-789"})

    def fake_popen(_cmd: list[str], **_kwargs: Any) -> _FakePopen:
        return _FakePopen(0, stdout_lines=[init_event, _result_event("ok", session_id="xyz-789")])

    monkeypatch.setattr(subprocess, "Popen", fake_popen)
    ClaudeCodeBackend().run("task", ws, model=None)

    sid_path = live_session_path(ws.root)
    assert not sid_path.exists(), "live_session file was not cleaned up after run"


def test_live_session_path_is_not_inside_workspace(tmp_path: Path) -> None:
    """The side-channel file must not be inside the workspace (would be git-tracked)."""
    ws_root = str(tmp_path / "my-worktree")
    path = live_session_path(ws_root)
    assert not str(path).startswith(str(tmp_path)), (
        "live_session path is inside the workspace — it could be git-tracked"
    )
    assert tempfile.gettempdir().replace("\\", "/") in str(path).replace("\\", "/"), (
        "live_session path should be under the system temp dir"
    )


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
