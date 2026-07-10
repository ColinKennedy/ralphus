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


def test_spec_parses_system_prompt_fields() -> None:
    spec = SessionSpec.from_json(
        _spec_json(
            prompt="do work",
            command=None,
            agent="claude-code",
            system_prompt="Follow the house style.",
            system_prompt_position="append",
        )
    )
    assert spec.system_prompt == "Follow the house style."
    assert spec.system_prompt_position == "append"


def test_spec_system_prompt_fields_default_none() -> None:
    spec = SessionSpec.from_json(_spec_json())
    assert spec.system_prompt is None
    assert spec.system_prompt_position is None


def test_spec_rejects_non_string_system_prompt() -> None:
    with pytest.raises(SpecError):
        SessionSpec.from_json(_spec_json(prompt="p", command=None, system_prompt=123))


def test_spec_verify_flag_defaults_false() -> None:
    spec = SessionSpec.from_json(_spec_json())
    assert spec.verify is False


def test_spec_parses_verify_flag() -> None:
    spec = SessionSpec.from_json(_spec_json(prompt="check it", command=None, verify=True))
    assert spec.verify is True


def test_spec_rejects_non_bool_verify() -> None:
    with pytest.raises(SpecError):
        SessionSpec.from_json(_spec_json(verify="yes"))


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

    def run(
        self,
        prompt: str,
        workspace: Workspace,
        *,
        model: str | None,
        append_system_prompt: str | None = None,
    ) -> BackendOutcome:
        workspace.write_file("agent_output.txt", f"prompt={prompt} model={model}")
        return BackendOutcome(summary="wrote a file", tokens_in=10, tokens_out=5, cost_usd=0.01)


class _FailingBackend:
    def run(
        self,
        prompt: str,
        workspace: Workspace,
        *,
        model: str | None,
        append_system_prompt: str | None = None,
    ) -> BackendOutcome:
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


def test_spec_parses_budget_tokens(tmp_path: Path) -> None:
    spec = SessionSpec.from_json(_spec_json(budget_tokens=5000, timeout_sec=120))
    assert spec.budget_tokens == 5000
    assert spec.timeout_sec == 120


def test_prompt_session_over_budget_fails(tmp_path: Path) -> None:
    # RAL-15: the mocked backend reports 15 tokens against a budget of 10, so the
    # session must fail with a budget message instead of reporting success.
    spec = SessionSpec.from_json(
        json.dumps(
            {
                "run_id": "r",
                "task": "t",
                "session_id": "s",
                "cwd": str(tmp_path),
                "prompt": "spend a lot",
                "budget_tokens": 10,
            }
        )
    )
    result = run_session(spec, backend=_WritingBackend())  # 10 in + 5 out = 15
    assert not result.ok
    assert "budget exceeded" in (result.error or "")


def test_prompt_session_within_budget_succeeds(tmp_path: Path) -> None:
    spec = SessionSpec.from_json(
        json.dumps(
            {
                "run_id": "r",
                "task": "t",
                "session_id": "s",
                "cwd": str(tmp_path),
                "prompt": "modest",
                "budget_tokens": 100,
            }
        )
    )
    result = run_session(spec, backend=_WritingBackend())  # 15 total <= 100
    assert result.ok


def test_agent_verify_over_budget_fails_closed(tmp_path: Path) -> None:
    # A verify step that blows its budget reports verified=False (fail closed)
    # even if the model's output contained a PASS marker.
    spec = SessionSpec.from_json(_verify_spec_json(tmp_path, budget_tokens=2))
    result = run_session(spec, backend=_ScriptedVerdictBackend("all good\nRALPHUS_VERIFY: PASS"))
    assert result.verified is False
    assert "budget exceeded" in result.summary


def test_prompt_session_backend_error(tmp_path: Path) -> None:
    spec = SessionSpec.from_json(
        json.dumps(
            {"run_id": "r", "task": "t", "session_id": "s", "cwd": str(tmp_path), "prompt": "x"}
        )
    )
    result = run_session(spec, backend=_FailingBackend())
    assert not result.ok
    assert "model backend error" in (result.error or "")


# ── agent-kind verify execution ──────────────────────────────────────────────


def _verify_spec_json(tmp_path: Path, **overrides: object) -> str:
    base: dict[str, object] = {
        "run_id": "r",
        "task": "t",
        "session_id": "s",
        "cwd": str(tmp_path),
        "prompt": "check that the build works",
        "verify": True,
    }
    base.update(overrides)
    return json.dumps(base)


class _ScriptedVerdictBackend:
    """A fake backend that returns a canned summary, for verdict parsing."""

    def __init__(self, summary: str) -> None:
        self._summary = summary

    def run(
        self,
        prompt: str,
        workspace: Workspace,
        *,
        model: str | None,
        append_system_prompt: str | None = None,
    ) -> BackendOutcome:
        return BackendOutcome(summary=self._summary, tokens_in=1, tokens_out=2, cost_usd=0.1)


def test_agent_verify_without_backend_fails(tmp_path: Path) -> None:
    spec = SessionSpec.from_json(_verify_spec_json(tmp_path))
    result = run_session(spec, backend=None)
    assert not result.ok
    assert result.verified is None
    assert "backend" in (result.error or "")


def test_agent_verify_pass_marker_sets_verified_true(tmp_path: Path) -> None:
    spec = SessionSpec.from_json(_verify_spec_json(tmp_path))
    result = run_session(spec, backend=_ScriptedVerdictBackend("looks good\nRALPHUS_VERIFY: PASS"))
    assert result.ok
    assert result.verified is True


def test_agent_verify_fail_marker_sets_verified_false(tmp_path: Path) -> None:
    spec = SessionSpec.from_json(_verify_spec_json(tmp_path))
    result = run_session(
        spec, backend=_ScriptedVerdictBackend("still broken\nRALPHUS_VERIFY: FAIL")
    )
    assert result.ok, "the verifier itself ran fine; it just found a failure"
    assert result.verified is False


def test_agent_verify_marker_embedded_mid_sentence_still_parses(tmp_path: Path) -> None:
    # Regression: a real local model (qwen3.5) put the marker inline instead of
    # alone on its own line as instructed — parsing must tolerate that instead
    # of failing closed on a technicality.
    spec = SessionSpec.from_json(_verify_spec_json(tmp_path))
    result = run_session(
        spec,
        backend=_ScriptedVerdictBackend("The check succeeded, confirming RALPHUS_VERIFY: PASS."),
    )
    assert result.ok
    assert result.verified is True


def test_agent_verify_trusts_the_last_marker_when_several_appear(tmp_path: Path) -> None:
    spec = SessionSpec.from_json(_verify_spec_json(tmp_path))
    result = run_session(
        spec,
        backend=_ScriptedVerdictBackend(
            "First I thought RALPHUS_VERIFY: FAIL, but on reflection RALPHUS_VERIFY: PASS"
        ),
    )
    assert result.ok
    assert result.verified is True


def test_agent_verify_missing_marker_fails_closed(tmp_path: Path) -> None:
    spec = SessionSpec.from_json(_verify_spec_json(tmp_path))
    result = run_session(spec, backend=_ScriptedVerdictBackend("I think it's fine, probably"))
    assert result.ok
    assert result.verified is False
    assert "no" in result.summary.lower() and "marker" in result.summary.lower()


def test_agent_verify_backend_error_leaves_verified_none(tmp_path: Path) -> None:
    spec = SessionSpec.from_json(_verify_spec_json(tmp_path))
    result = run_session(spec, backend=_FailingBackend())
    assert not result.ok
    assert result.verified is None


def test_agent_verify_sends_verdict_instructions_as_system_prompt(tmp_path: Path) -> None:
    seen_prompts: list[str] = []
    seen_system: list[str | None] = []

    class _RecordingBackend:
        def run(
            self,
            prompt: str,
            workspace: Workspace,
            *,
            model: str | None,
            append_system_prompt: str | None = None,
        ) -> BackendOutcome:
            seen_prompts.append(prompt)
            seen_system.append(append_system_prompt)
            return BackendOutcome(summary="RALPHUS_VERIFY: PASS")

    spec = SessionSpec.from_json(_verify_spec_json(tmp_path, prompt="check the widget"))
    run_session(spec, backend=_RecordingBackend())
    assert len(seen_prompts) == 1
    assert seen_prompts[0] == "check the widget"
    assert seen_system[0] is not None
    assert "RALPHUS_VERIFY: PASS" in seen_system[0]
    assert "RALPHUS_VERIFY: FAIL" in seen_system[0]


# ── SessionResult ────────────────────────────────────────────────────────────


def test_result_json_roundtrip() -> None:
    result = SessionResult.done(summary="ok", tokens_in=3, tokens_out=4, cost_usd=0.5)
    data = json.loads(result.to_json())
    assert data["status"] == "done"
    assert data["tokens_in"] == 3
    assert data["error"] is None
    assert data["verified"] is None


def test_result_json_roundtrip_with_verified() -> None:
    result = SessionResult.done(summary="ok", verified=True)
    data = json.loads(result.to_json())
    assert data["verified"] is True


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
