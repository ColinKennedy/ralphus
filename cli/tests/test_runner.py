"""Tests for the ralphus session runner."""

from __future__ import annotations

import json
from pathlib import Path

import pytest

from ralphus.runner.backend import BackendError, BackendOutcome
from ralphus.runner.execute import run_session
from ralphus.runner.spec import SessionResult, SessionSpec, SpecError
from ralphus.runner.tools import ToolError, Workspace

# ── SessionSpec parsing ──────────────────────────────────────────────────────


def _spec_json(**overrides: object) -> str:
    base: dict[str, object] = {
        "run_id": "run-1",
        "task": "t",
        "session_id": "session-0",
        "cwd": "/tmp",
        "command": "echo hi",
    }
    base.update(overrides)
    return json.dumps(base)


def test_spec_parses_command_session() -> None:
    spec = SessionSpec.from_json(_spec_json())
    assert spec.command == "echo hi"
    assert spec.prompt is None
    assert spec.agent == "claude"


def test_spec_rejects_both_prompt_and_command() -> None:
    with pytest.raises(SpecError):
        SessionSpec.from_json(_spec_json(prompt="do it"))


def test_spec_rejects_neither_prompt_nor_command() -> None:
    with pytest.raises(SpecError):
        SessionSpec.from_json(
            json.dumps({"run_id": "r", "task": "t", "session_id": "s", "cwd": "/tmp"})
        )


def test_spec_rejects_bad_json() -> None:
    with pytest.raises(SpecError):
        SessionSpec.from_json("{not json")


def test_spec_requires_string_fields() -> None:
    with pytest.raises(SpecError):
        SessionSpec.from_json(
            json.dumps({"run_id": 1, "task": "t", "session_id": "s", "cwd": "/tmp", "command": "x"})
        )


# ── Workspace tools ──────────────────────────────────────────────────────────


def test_workspace_write_then_read(tmp_path: Path) -> None:
    ws = Workspace.create(str(tmp_path))
    ws.write_file("sub/hello.txt", "world")
    assert ws.read_file("sub/hello.txt") == "world"


def test_workspace_rejects_escape(tmp_path: Path) -> None:
    ws = Workspace.create(str(tmp_path))
    with pytest.raises(ToolError):
        ws.write_file("../escape.txt", "nope")


def test_workspace_missing_dir_raises() -> None:
    with pytest.raises(ToolError):
        Workspace.create("/definitely/not/a/real/dir/ralphus")


def test_run_bash_reports_exit_code(tmp_path: Path) -> None:
    ws = Workspace.create(str(tmp_path))
    ok = ws.run_bash("exit 0")
    assert ok.ok
    bad = ws.run_bash("exit 3")
    assert not bad.ok
    assert bad.exit_code == 3


# ── Session execution ────────────────────────────────────────────────────────


def test_command_session_that_writes_a_file(tmp_path: Path) -> None:
    spec = SessionSpec.from_json(_spec_json(cwd=str(tmp_path), command="echo produced > out.txt"))
    result = run_session(spec)
    assert result.ok, result.error
    assert (tmp_path / "out.txt").exists()


def test_command_session_failure(tmp_path: Path) -> None:
    spec = SessionSpec.from_json(_spec_json(cwd=str(tmp_path), command="exit 7"))
    result = run_session(spec)
    assert not result.ok
    assert "exited 7" in (result.error or "")


def test_prompt_session_without_backend_fails(tmp_path: Path) -> None:
    spec = SessionSpec.from_json(
        json.dumps(
            {"run_id": "r", "task": "t", "session_id": "s", "cwd": str(tmp_path), "prompt": "do it"}
        )
    )
    result = run_session(spec, backend=None)
    assert not result.ok
    assert "backend" in (result.error or "")


class _WritingBackend:
    """A fake backend that proves tool use by writing a file, then reports usage."""

    def run(self, prompt: str, workspace: Workspace, *, model: str | None) -> BackendOutcome:
        workspace.write_file("agent_output.txt", f"prompt={prompt} model={model}")
        return BackendOutcome(summary="wrote a file", tokens_in=10, tokens_out=5, cost_usd=0.01)


class _FailingBackend:
    def run(self, prompt: str, workspace: Workspace, *, model: str | None) -> BackendOutcome:
        raise BackendError("model unreachable")


def test_prompt_session_with_backend(tmp_path: Path) -> None:
    spec = SessionSpec.from_json(
        json.dumps(
            {
                "run_id": "r",
                "task": "t",
                "session_id": "s",
                "cwd": str(tmp_path),
                "prompt": "make a file",
                "model": "qwen2.5-coder",
            }
        )
    )
    result = run_session(spec, backend=_WritingBackend())
    assert result.ok
    assert result.tokens_in == 10
    assert result.cost_usd == pytest.approx(0.01)
    assert (tmp_path / "agent_output.txt").read_text(
        encoding="utf-8"
    ) == "prompt=make a file model=qwen2.5-coder"


def test_prompt_session_backend_error(tmp_path: Path) -> None:
    spec = SessionSpec.from_json(
        json.dumps(
            {"run_id": "r", "task": "t", "session_id": "s", "cwd": str(tmp_path), "prompt": "x"}
        )
    )
    result = run_session(spec, backend=_FailingBackend())
    assert not result.ok
    assert "model backend error" in (result.error or "")


# ── SessionResult ────────────────────────────────────────────────────────────


def test_result_json_roundtrip() -> None:
    result = SessionResult.done(summary="ok", tokens_in=3, tokens_out=4, cost_usd=0.5)
    data = json.loads(result.to_json())
    assert data["status"] == "done"
    assert data["tokens_in"] == 3
    assert data["error"] is None


# ── __main__ ─────────────────────────────────────────────────────────────────


def test_main_runs_command_spec_from_file(
    tmp_path: Path, capsys: pytest.CaptureFixture[str]
) -> None:
    from ralphus.runner.__main__ import main

    spec_path = tmp_path / "spec.json"
    spec_path.write_text(
        _spec_json(cwd=str(tmp_path), command="echo done > result.txt"), encoding="utf-8"
    )
    code = main([str(spec_path)])
    captured = capsys.readouterr()
    payload = json.loads(captured.out)
    assert code == 0
    assert payload["status"] == "done"
    assert (tmp_path / "result.txt").exists()


def test_main_reports_bad_spec(tmp_path: Path, capsys: pytest.CaptureFixture[str]) -> None:
    from ralphus.runner.__main__ import main

    spec_path = tmp_path / "spec.json"
    spec_path.write_text("{bad", encoding="utf-8")
    code = main([str(spec_path)])
    captured = capsys.readouterr()
    assert code == 1
    assert json.loads(captured.out)["status"] == "failed"


def test_main_native_prompt_without_pydantic_ai_is_clear(
    tmp_path: Path, capsys: pytest.CaptureFixture[str], monkeypatch: pytest.MonkeyPatch
) -> None:
    import ralphus.runner.__main__ as runner_main

    # Simulate pydantic-ai being unavailable for a native (ollama) prompt session.
    monkeypatch.setattr(runner_main, "_load_backend", lambda _agent, _args: None)
    spec_path = tmp_path / "spec.json"
    spec_path.write_text(
        json.dumps(
            {
                "run_id": "r",
                "task": "t",
                "session_id": "s",
                "cwd": str(tmp_path),
                "agent": "ollama",
                "prompt": "do it",
            }
        ),
        encoding="utf-8",
    )
    code = runner_main.main([str(spec_path)])
    payload = json.loads(capsys.readouterr().out)
    assert code == 1
    assert payload["status"] == "failed"
    assert "runner" in payload["error"] and "pydantic-ai" in payload["error"]
