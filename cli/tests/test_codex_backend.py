"""Tests for the Codex CLI backend (command construction, no real CLI)."""

from __future__ import annotations

import io
import json
import shutil
import subprocess
from pathlib import Path
from typing import Any

import pytest

from ralphus.runner.backend import BackendError
from ralphus.runner.cli_agent_common import live_session_path
from ralphus.runner.cli_agent_common import write_prompt_file as _write_prompt_file
from ralphus.runner.codex_backend import CodexBackend
from ralphus.runner.tools import Workspace


def _thread_started(thread_id: str) -> str:
    return json.dumps({"type": "thread.started", "thread_id": thread_id})


def _agent_message(text: str) -> str:
    return json.dumps(
        {"type": "item.completed", "item": {"id": "item0", "type": "agent_message", "text": text}}
    )


def _turn_completed(tokens_in: int = 0, tokens_out: int = 0) -> str:
    return json.dumps(
        {
            "type": "turn.completed",
            "usage": {
                "input_tokens": tokens_in,
                "cached_input_tokens": 0,
                "cache_write_input_tokens": 0,
                "output_tokens": tokens_out,
                "reasoning_output_tokens": 0,
            },
        }
    )


def _turn_failed(message: str) -> str:
    return json.dumps({"type": "turn.failed", "error": {"message": message}})


class _FakeStdin(io.StringIO):
    """Records what was written before the writer thread closes it."""

    def __init__(self) -> None:
        super().__init__()
        self.written = ""

    def write(self, s: str) -> int:
        self.written += s
        return super().write(s)


class _FakePopen:
    """Minimal subprocess.Popen stand-in for Codex's `--json` JSONL output."""

    def __init__(
        self,
        returncode: int = 0,
        stdout_lines: list[str] | None = None,
        stderr_str: str = "",
    ) -> None:
        self.returncode = returncode
        self.stdout: list[str] = [line + "\n" for line in (stdout_lines or [])]
        self.stderr = io.StringIO(stderr_str)
        self.stdin = _FakeStdin()

    def wait(self, timeout: float | None = None) -> int:
        return self.returncode


def _cmd_prompt_arg_is_stdin_sentinel(cmd: list[str]) -> None:
    assert cmd[-1] == "-", f"expected trailing '-' stdin sentinel, got {cmd[-1]!r}"


def test_builds_exec_command_with_json_and_no_ephemeral(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    ws = Workspace.create(str(tmp_path))
    monkeypatch.setattr(shutil, "which", lambda _program: None)
    captured: dict[str, Any] = {}

    def fake_popen(cmd: list[str], **kwargs: Any) -> _FakePopen:
        captured["cmd"] = cmd
        captured["cwd"] = kwargs.get("cwd")
        return _FakePopen(0, stdout_lines=[_agent_message("did the thing")])

    monkeypatch.setattr(subprocess, "Popen", fake_popen)
    outcome = CodexBackend().run("do the thing", ws, model="o4-mini")

    assert outcome.summary == "did the thing"
    cmd = captured["cmd"]
    assert cmd[0] == "codex"
    assert cmd[1] == "exec"
    assert "--json" in cmd
    assert "--ephemeral" not in cmd, "ephemeral runs can't be resumed later"
    assert "--dangerously-bypass-approvals-and-sandbox" in cmd
    assert "--skip-git-repo-check" in cmd
    assert cmd[cmd.index("-C") + 1] == str(ws.root)
    assert cmd[cmd.index("-m") + 1] == "o4-mini"
    _cmd_prompt_arg_is_stdin_sentinel(cmd)
    assert captured["cwd"] == ws.root


def test_prompt_is_passed_via_stdin(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    ws = Workspace.create(str(tmp_path))
    monkeypatch.setattr(shutil, "which", lambda _program: None)
    captured: dict[str, Any] = {}

    def fake_popen(cmd: list[str], **_kwargs: Any) -> _FakePopen:
        popen = _FakePopen(0)
        captured["popen"] = popen
        captured["cmd"] = cmd
        return popen

    monkeypatch.setattr(subprocess, "Popen", fake_popen)
    CodexBackend().run("my prompt text", ws, model=None)

    assert captured["popen"].stdin.written == "my prompt text"
    assert "my prompt text" not in captured["cmd"]


def test_prompt_file_is_removed_after_run(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    ws = Workspace.create(str(tmp_path))
    monkeypatch.setattr(shutil, "which", lambda _program: None)
    monkeypatch.setattr(Path, "home", lambda: tmp_path)
    monkeypatch.delenv("RALPHUS_CONFIGURATION_PATH", raising=False)

    def fake_popen(_cmd: list[str], **_kwargs: Any) -> _FakePopen:
        return _FakePopen(0)

    monkeypatch.setattr(subprocess, "Popen", fake_popen)
    CodexBackend().run("hello", ws, model=None)

    leftover = list((tmp_path / ".ralphus" / "task_prompts").glob("*.md"))
    assert leftover == [], f"prompt file was not cleaned up: {leftover}"


def test_prompt_file_is_removed_even_on_error(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    ws = Workspace.create(str(tmp_path))
    monkeypatch.setattr(shutil, "which", lambda _program: None)
    monkeypatch.setattr(Path, "home", lambda: tmp_path)
    monkeypatch.delenv("RALPHUS_CONFIGURATION_PATH", raising=False)

    def fake_popen(_cmd: list[str], **_kwargs: Any) -> _FakePopen:
        raise OSError("simulated failure")

    monkeypatch.setattr(subprocess, "Popen", fake_popen)
    with pytest.raises(BackendError):
        CodexBackend().run("hello", ws, model=None)

    leftover = list((tmp_path / ".ralphus" / "task_prompts").glob("*.md"))
    assert leftover == [], f"prompt file was not cleaned up after error: {leftover}"


def test_no_model_flag_when_unset(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    ws = Workspace.create(str(tmp_path))
    monkeypatch.setattr(shutil, "which", lambda _program: None)
    captured: dict[str, Any] = {}

    def fake_popen(cmd: list[str], **_kwargs: Any) -> _FakePopen:
        captured["cmd"] = cmd
        return _FakePopen(0)

    monkeypatch.setattr(subprocess, "Popen", fake_popen)
    CodexBackend().run("x", ws, model=None)
    assert "-m" not in captured["cmd"]


def test_program_is_overridable(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    ws = Workspace.create(str(tmp_path))
    monkeypatch.setenv("RALPHUS_CODEX_CMD", "my-codex")
    monkeypatch.setattr(shutil, "which", lambda _program: None)
    captured: dict[str, Any] = {}

    def fake_popen(cmd: list[str], **_kwargs: Any) -> _FakePopen:
        captured["cmd"] = cmd
        return _FakePopen(0)

    monkeypatch.setattr(subprocess, "Popen", fake_popen)
    CodexBackend().run("x", ws, model=None)
    assert captured["cmd"][0] == "my-codex"


def test_developer_instructions_flag_precedes_exec(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """`-c developer_instructions=...` is Codex's closest analog to
    `--append-system-prompt`. It must appear *before* `exec` on the command
    line -- the `exec` subcommand's own CLI struct skips re-declaring `-c`
    and only picks it up when threaded in from what was parsed before the
    subcommand token (see codex_backend.py's module docstring)."""
    ws = Workspace.create(str(tmp_path))
    monkeypatch.setattr(shutil, "which", lambda _program: None)
    captured: dict[str, Any] = {}

    def fake_popen(cmd: list[str], **_kwargs: Any) -> _FakePopen:
        captured["cmd"] = cmd
        return _FakePopen(0, stdout_lines=[_agent_message("ok")])

    monkeypatch.setattr(subprocess, "Popen", fake_popen)
    CodexBackend().run("do work", ws, model=None, append_system_prompt="Follow the house style.")

    cmd = captured["cmd"]
    assert cmd[cmd.index("-c") + 1] == "developer_instructions=Follow the house style."
    assert cmd.index("-c") < cmd.index("exec"), "-c must precede exec to be picked up"


def test_no_developer_instructions_flag_when_unset(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    ws = Workspace.create(str(tmp_path))
    monkeypatch.setattr(shutil, "which", lambda _program: None)
    captured: dict[str, Any] = {}

    def fake_popen(cmd: list[str], **_kwargs: Any) -> _FakePopen:
        captured["cmd"] = cmd
        return _FakePopen(0)

    monkeypatch.setattr(subprocess, "Popen", fake_popen)
    CodexBackend().run("x", ws, model=None)
    assert "-c" not in captured["cmd"]


def test_resume_maps_to_resume_subcommand_and_replaces_prompt(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """`resume_agent_session_id` (holding a Codex thread id) becomes
    `exec resume <id> -` and swaps in a short continuation directive instead
    of re-sending the original prompt, matching ClaudeCodeBackend's same
    resume behavior."""
    ws = Workspace.create(str(tmp_path))
    monkeypatch.setattr(shutil, "which", lambda _program: None)
    captured: dict[str, Any] = {}

    def fake_popen(cmd: list[str], **_kwargs: Any) -> _FakePopen:
        popen = _FakePopen(0, stdout_lines=[_agent_message("resumed and finished")])
        captured["popen"] = popen
        captured["cmd"] = cmd
        return popen

    monkeypatch.setattr(subprocess, "Popen", fake_popen)
    outcome = CodexBackend().run(
        "the original long task prompt",
        ws,
        model=None,
        resume_agent_session_id="dropped-thread-abc",
    )

    assert outcome.summary == "resumed and finished"
    cmd = captured["cmd"]
    assert cmd[-3:] == ["resume", "dropped-thread-abc", "-"]
    prompt_sent = captured["popen"].stdin.written
    assert prompt_sent != "the original long task prompt"
    assert "continue" in prompt_sent.lower()


def test_no_resume_when_unset(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    ws = Workspace.create(str(tmp_path))
    monkeypatch.setattr(shutil, "which", lambda _program: None)
    captured: dict[str, Any] = {}

    def fake_popen(cmd: list[str], **_kwargs: Any) -> _FakePopen:
        captured["cmd"] = cmd
        return _FakePopen(0)

    monkeypatch.setattr(subprocess, "Popen", fake_popen)
    CodexBackend().run("x", ws, model=None)
    assert "resume" not in captured["cmd"]
    assert captured["cmd"][-1] == "-"


def test_thread_id_written_to_live_session_file_and_returned(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch, capsys: pytest.CaptureFixture[str]
) -> None:
    ws = Workspace.create(str(tmp_path))
    monkeypatch.setattr(shutil, "which", lambda _program: None)

    def fake_popen(_cmd: list[str], **_kwargs: Any) -> _FakePopen:
        return _FakePopen(
            0, stdout_lines=[_thread_started("thread-abc-123"), _agent_message("done")]
        )

    written_sid: list[str] = []
    original_write = Path.write_text

    def spy_write(self: Path, text: str, **kwargs: Any) -> None:
        if self.suffix == ".live_session":
            written_sid.append(text)
        original_write(self, text, **kwargs)

    monkeypatch.setattr(Path, "write_text", spy_write)
    monkeypatch.setattr(subprocess, "Popen", fake_popen)
    outcome = CodexBackend().run("task", ws, model=None)

    assert outcome.agent_session_id == "thread-abc-123"
    assert "thread-abc-123" in written_sid, "thread id was not written to the live_session file"
    stderr = capsys.readouterr().err
    assert "RALPHUS_EVENT: " in stderr
    event = json.loads(stderr.split("RALPHUS_EVENT: ", 1)[1].splitlines()[0])
    assert event["payload"]["agent_session_id"] == "thread-abc-123"


def test_live_session_file_cleaned_up_after_run(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    ws = Workspace.create(str(tmp_path))
    monkeypatch.setattr(shutil, "which", lambda _program: None)

    def fake_popen(_cmd: list[str], **_kwargs: Any) -> _FakePopen:
        return _FakePopen(0, stdout_lines=[_thread_started("xyz-789"), _agent_message("ok")])

    monkeypatch.setattr(subprocess, "Popen", fake_popen)
    CodexBackend().run("task", ws, model=None)

    sid_path = live_session_path(ws.root)
    assert not sid_path.exists(), "live_session file was not cleaned up after run"


def test_usage_parsed_from_turn_completed(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    ws = Workspace.create(str(tmp_path))
    monkeypatch.setattr(shutil, "which", lambda _program: None)

    def fake_popen(_cmd: list[str], **_kwargs: Any) -> _FakePopen:
        return _FakePopen(
            0,
            stdout_lines=[_agent_message("done"), _turn_completed(tokens_in=123, tokens_out=45)],
        )

    monkeypatch.setattr(subprocess, "Popen", fake_popen)
    outcome = CodexBackend().run("task", ws, model=None)

    assert outcome.tokens_in == 123
    assert outcome.tokens_out == 45
    # Codex's JSON output has no dollar-cost field anywhere -- unlike Claude
    # Code's total_cost_usd, this is always 0.0 for this backend.
    assert outcome.cost_usd == 0.0


def test_usage_accumulates_across_multiple_turns(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """RAL-187: each `turn.completed` reports only *that turn's* usage, so a
    multi-turn `codex exec` must sum them. Overwriting made the board report
    just the final turn -- the "Codex shows 0 tokens" symptom."""
    ws = Workspace.create(str(tmp_path))
    monkeypatch.setattr(shutil, "which", lambda _program: None)

    def fake_popen(_cmd: list[str], **_kwargs: Any) -> _FakePopen:
        return _FakePopen(
            0,
            stdout_lines=[
                _turn_completed(tokens_in=100, tokens_out=10),
                _turn_completed(tokens_in=250, tokens_out=25),
                _agent_message("done"),
                _turn_completed(tokens_in=7, tokens_out=3),
            ],
        )

    monkeypatch.setattr(subprocess, "Popen", fake_popen)
    outcome = CodexBackend().run("task", ws, model=None)

    assert outcome.tokens_in == 357
    assert outcome.tokens_out == 38


def test_usage_survives_a_turn_that_fails_after_an_earlier_one_completed(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """RAL-187: a run whose last turn fails still reports what the earlier,
    completed turns actually spent -- those tokens were really burned."""
    ws = Workspace.create(str(tmp_path))
    monkeypatch.setattr(shutil, "which", lambda _program: None)

    def fake_popen(_cmd: list[str], **_kwargs: Any) -> _FakePopen:
        return _FakePopen(
            0,
            stdout_lines=[
                _turn_completed(tokens_in=80, tokens_out=9),
                _turn_failed("model exhausted its context"),
            ],
        )

    monkeypatch.setattr(subprocess, "Popen", fake_popen)
    outcome = CodexBackend().run("task", ws, model=None)

    assert outcome.tokens_in == 80
    assert outcome.tokens_out == 9


def _live_usage_events(stderr: str) -> list[dict[str, Any]]:
    """The payloads of every `codex live usage` RALPHUS_EVENT line, in order."""
    marker = "RALPHUS_EVENT: "
    events = [
        json.loads(line[len(marker) :]) for line in stderr.splitlines() if line.startswith(marker)
    ]
    return [ev["payload"] for ev in events if ev["message"] == "codex live usage"]


def test_live_usage_emitted_per_completed_turn(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch, capsys: pytest.CaptureFixture[str]
) -> None:
    """RAL-187: the board read 0 tokens for the whole of a running Codex
    session because usage only reached the daemon at the very end. Each
    completed turn now forwards the running total over the RALPHUS_EVENT
    stderr channel, which `runner.rs` already persists to the session row."""
    ws = Workspace.create(str(tmp_path))
    monkeypatch.setattr(shutil, "which", lambda _program: None)

    def fake_popen(_cmd: list[str], **_kwargs: Any) -> _FakePopen:
        return _FakePopen(
            0,
            stdout_lines=[
                _turn_completed(tokens_in=100, tokens_out=10),
                _turn_completed(tokens_in=250, tokens_out=25),
            ],
        )

    monkeypatch.setattr(subprocess, "Popen", fake_popen)
    CodexBackend().run("task", ws, model=None)

    payloads = _live_usage_events(capsys.readouterr().err)
    assert [(p["tokens_in"], p["tokens_out"]) for p in payloads] == [(100, 10), (350, 35)]
    # Codex reports no dollar cost anywhere; the live event must not invent
    # one -- the board renders a reported zero as "N/A" instead.
    assert all(p["cost_usd"] == 0.0 for p in payloads)


def test_summary_keeps_tail_not_head_of_long_agent_message(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """A RALPHUS_VERIFY: marker on the model's final output line must survive
    truncation -- so the summary keeps the trailing chars, not the leading
    ones, unlike Claude Code's own (front-truncated) `result` field."""
    ws = Workspace.create(str(tmp_path))
    monkeypatch.setattr(shutil, "which", lambda _program: None)
    long_message = ("x" * 3000) + "\nRALPHUS_VERIFY: PASS"

    def fake_popen(_cmd: list[str], **_kwargs: Any) -> _FakePopen:
        return _FakePopen(0, stdout_lines=[_agent_message(long_message)])

    monkeypatch.setattr(subprocess, "Popen", fake_popen)
    outcome = CodexBackend().run("task", ws, model=None)

    assert outcome.summary.endswith("RALPHUS_VERIFY: PASS")
    assert len(outcome.summary) <= 2000


def test_turn_failed_raises_backend_error(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    ws = Workspace.create(str(tmp_path))
    monkeypatch.setattr(shutil, "which", lambda _program: None)

    def fake_popen(_cmd: list[str], **_kwargs: Any) -> _FakePopen:
        return _FakePopen(1, stdout_lines=[_turn_failed("model exhausted its context")])

    monkeypatch.setattr(subprocess, "Popen", fake_popen)
    with pytest.raises(BackendError, match="model exhausted its context"):
        CodexBackend().run("x", ws, model=None)


def test_nonzero_exit_raises(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    ws = Workspace.create(str(tmp_path))

    def fake_popen(_cmd: list[str], **_kwargs: Any) -> _FakePopen:
        return _FakePopen(1, stderr_str="codex failed")

    monkeypatch.setattr(subprocess, "Popen", fake_popen)
    with pytest.raises(BackendError, match="exited 1"):
        CodexBackend().run("x", ws, model=None)


def test_oserror_raises_backend_error(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    ws = Workspace.create(str(tmp_path))
    monkeypatch.setattr(shutil, "which", lambda _program: None)

    def fake_popen(_cmd: list[str], **_kwargs: Any) -> _FakePopen:
        raise OSError("no such file")

    monkeypatch.setattr(subprocess, "Popen", fake_popen)
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
