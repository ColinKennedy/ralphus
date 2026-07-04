"""Smoke tests for the ralphus CLI skeleton."""

from __future__ import annotations

import pytest

from ralphus import __version__
from ralphus.__main__ import build_parser, main


def test_version_is_set() -> None:
    assert __version__


def test_no_command_prints_help_and_succeeds(capsys: pytest.CaptureFixture[str]) -> None:
    code = main([])
    captured = capsys.readouterr()
    assert code == 0
    assert "usage: ralphus" in captured.out.lower()


def test_doctor_runs(capsys: pytest.CaptureFixture[str]) -> None:
    # Point at an unreachable daemon so the outcome is deterministic (daemon
    # check fails). Real check behaviour is covered in test_doctor.py.
    code = main(["--daemon-url", "http://127.0.0.1:9", "doctor"])
    captured = capsys.readouterr()
    assert code == 1
    assert "daemon" in captured.out.lower()


def test_version_flag_exits_zero() -> None:
    parser = build_parser()
    with pytest.raises(SystemExit) as excinfo:
        parser.parse_args(["--version"])
    assert excinfo.value.code == 0


def test_task_show_tutor_prints_reference(capsys: pytest.CaptureFixture[str]) -> None:
    code = main(["task", "show-tutor"])
    captured = capsys.readouterr()
    assert code == 0
    out = captured.out
    # Reflects ralphus-specific syntax, not the predecessor's.
    assert "[[task.session.review]]" in out
    assert "<<upstream>>" in out
    assert "{handoff:" in out
    assert "[[task]]" in out


def test_bare_task_prints_task_help(capsys: pytest.CaptureFixture[str]) -> None:
    code = main(["task"])
    captured = capsys.readouterr()
    assert code == 0
    assert "show-tutor" in captured.out
