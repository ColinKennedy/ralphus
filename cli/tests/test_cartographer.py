from __future__ import annotations

import json

import pytest

from ralphus.runner import cartographer


def test_emit_writes_marker_prefixed_json_to_stderr(capsys: pytest.CaptureFixture[str]) -> None:
    cartographer.emit("llm", "session start", scope="session", run_id="run-1", session_id="s0")
    captured = capsys.readouterr()
    assert captured.out == ""
    line = captured.err.strip()
    assert line.startswith("RALPHUS_EVENT: ")
    event = json.loads(line.removeprefix("RALPHUS_EVENT: "))
    assert event == {
        "source": "llm",
        "message": "session start",
        "level": "info",
        "scope": "session",
        "run_id": "run-1",
        "session_id": "s0",
        "task": None,
        "payload": {},
    }


def test_emit_defaults_level_to_info_and_payload_to_empty_dict(
    capsys: pytest.CaptureFixture[str],
) -> None:
    cartographer.emit("runner", "hello")
    event = json.loads(capsys.readouterr().err.strip().removeprefix("RALPHUS_EVENT: "))
    assert event["level"] == "info"
    assert event["payload"] == {}
    assert event["run_id"] is None


def test_emit_carries_arbitrary_payload(capsys: pytest.CaptureFixture[str]) -> None:
    cartographer.emit(
        "llm-invoke",
        "done",
        level="debug",
        payload={"tokens_in": 5, "tokens_out": 10, "nested": {"a": 1}},
    )
    event = json.loads(capsys.readouterr().err.strip().removeprefix("RALPHUS_EVENT: "))
    assert event["level"] == "debug"
    assert event["payload"] == {"tokens_in": 5, "tokens_out": 10, "nested": {"a": 1}}
