"""Tests for `ralphus doctor`."""

from __future__ import annotations

import pytest

import ralphus.__main__ as cli
from ralphus.doctor import CheckResult, run_checks

# A port that refuses connections, so the daemon check deterministically fails.
UNREACHABLE = "http://127.0.0.1:9"


def test_run_checks_shape() -> None:
    results = run_checks(UNREACHABLE)
    names = {r.name for r in results}
    assert {"daemon", "git", "runner", "pydantic-ai", "ollama"} <= names
    assert all(r.status in {"pass", "warn", "fail"} for r in results)


def test_daemon_down_is_a_failure() -> None:
    results = run_checks(UNREACHABLE)
    daemon = next(r for r in results if r.name == "daemon")
    assert daemon.is_fail


def test_check_result_is_fail() -> None:
    assert CheckResult("x", "fail", "").is_fail
    assert not CheckResult("x", "warn", "").is_fail
    assert not CheckResult("x", "pass", "").is_fail


def test_cli_doctor_exit_code_when_daemon_down(capsys: pytest.CaptureFixture[str]) -> None:
    code = cli.main(["--daemon-url", UNREACHABLE, "doctor"])
    out = capsys.readouterr().out
    assert code == 1
    assert "daemon" in out
    assert "FAIL" in out
