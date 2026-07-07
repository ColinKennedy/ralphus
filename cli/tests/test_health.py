"""Tests for `ralphus check health`."""

from __future__ import annotations

import pytest

import ralphus.__main__ as cli
from ralphus.health import DEVELOPER, CheckResult, run_checks

# A port that refuses connections, so the daemon check deterministically fails.
UNREACHABLE = "http://127.0.0.1:9"


def test_run_checks_shape() -> None:
    results = run_checks(UNREACHABLE)
    names = {r.name for r in results}
    assert {"daemon", "git", "runner", "ollama", "nvidia-smi", "config"} <= names
    assert all(r.status in {"pass", "warn", "fail"} for r in results)


def test_nvidia_smi_missing_is_a_warning_not_a_failure(monkeypatch: pytest.MonkeyPatch) -> None:
    # No nvidia-smi on PATH: the GPU column degrades gracefully, so this warns
    # rather than failing `check health`.
    monkeypatch.setattr("ralphus.health.shutil.which", lambda _name: None)
    results = run_checks(UNREACHABLE)
    nvidia = next(r for r in results if r.name == "nvidia-smi")
    assert nvidia.status == "warn"
    assert not nvidia.is_fail


def test_developer_checks_are_off_by_default() -> None:
    names = {r.name for r in run_checks(UNREACHABLE)}
    assert "cargo" not in names
    assert "pydantic-ai" not in names


def test_developer_checks_opt_in() -> None:
    results = run_checks(UNREACHABLE, enable_developer_checks=True)
    developer = {r.name for r in results if r.section == DEVELOPER}
    assert {"cargo", "pydantic-ai"} <= developer


def test_daemon_down_is_a_failure() -> None:
    results = run_checks(UNREACHABLE)
    daemon = next(r for r in results if r.name == "daemon")
    assert daemon.is_fail


def test_ollama_unreachable_is_a_failure(monkeypatch: pytest.MonkeyPatch) -> None:
    # Point ollama at a refused port so the check deterministically fails.
    monkeypatch.setenv("RALPHUS_OLLAMA_URL", "http://127.0.0.1:9/v1")
    results = run_checks(UNREACHABLE)
    ollama = next(r for r in results if r.name == "ollama")
    assert ollama.is_fail


def test_cargo_missing_is_a_developer_failure(monkeypatch: pytest.MonkeyPatch) -> None:
    monkeypatch.setattr("ralphus.health.shutil.which", lambda _name: None)
    results = run_checks(UNREACHABLE, enable_developer_checks=True)
    cargo = next(r for r in results if r.name == "cargo")
    assert cargo.is_fail
    assert cargo.section == DEVELOPER
    assert "cargo" in cargo.detail


def test_pydantic_ai_is_in_the_developer_section() -> None:
    results = run_checks(UNREACHABLE, enable_developer_checks=True)
    pydantic_ai = next(r for r in results if r.name == "pydantic-ai")
    assert pydantic_ai.section == DEVELOPER


def test_pydantic_ai_missing_is_a_developer_failure(monkeypatch: pytest.MonkeyPatch) -> None:
    monkeypatch.setattr("ralphus.health.pydantic_ai_available", lambda: False)
    results = run_checks(UNREACHABLE, enable_developer_checks=True)
    pydantic_ai = next(r for r in results if r.name == "pydantic-ai")
    assert pydantic_ai.is_fail


def test_config_negative_timeout_is_a_failure(monkeypatch: pytest.MonkeyPatch) -> None:
    from ralphus.config import Config, TaskConfig

    monkeypatch.setattr(
        "ralphus.health.load_config", lambda: Config(task=TaskConfig(maximum_timeout_seconds=-1))
    )
    results = run_checks(UNREACHABLE)
    config = next(r for r in results if r.name == "config")
    assert config.is_fail
    assert "-1" in config.detail


def test_config_zero_timeout_is_a_warning(monkeypatch: pytest.MonkeyPatch) -> None:
    from ralphus.config import Config, TaskConfig

    monkeypatch.setattr(
        "ralphus.health.load_config", lambda: Config(task=TaskConfig(maximum_timeout_seconds=0))
    )
    results = run_checks(UNREACHABLE)
    config = next(r for r in results if r.name == "config")
    assert config.status == "warn"
    assert not config.is_fail


def test_config_positive_timeout_passes(monkeypatch: pytest.MonkeyPatch) -> None:
    from ralphus.config import Config, TaskConfig

    monkeypatch.setattr(
        "ralphus.health.load_config", lambda: Config(task=TaskConfig(maximum_timeout_seconds=3600))
    )
    results = run_checks(UNREACHABLE)
    config = next(r for r in results if r.name == "config")
    assert config.status == "pass"
    assert "3600" in config.detail


def test_check_result_is_fail() -> None:
    assert CheckResult("x", "fail", "").is_fail
    assert not CheckResult("x", "warn", "").is_fail
    assert not CheckResult("x", "pass", "").is_fail


def test_cli_check_health_exit_code_when_daemon_down(capsys: pytest.CaptureFixture[str]) -> None:
    code = cli.main(["--daemon-url", UNREACHABLE, "check", "health"])
    out = capsys.readouterr().out
    assert code == 1
    assert "daemon" in out
    assert "FAIL" in out
