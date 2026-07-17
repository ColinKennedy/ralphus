"""Tests for `ralphus check health`."""

from __future__ import annotations

import os
import shutil
import subprocess
from pathlib import Path
from typing import Any

import pytest

import ralphus.__main__ as cli
from ralphus.health import (
    DEVELOPER,
    CheckResult,
    is_compound_shell_command,
    run_checks,
    unquote_path,
)

# Every test here calls run_checks()/cli.main(), which always probes a real
# (refused) daemon socket and a real Ollama endpoint with their own network
# timeouts — under the RAL-94 bench harness's repeated in-process invocation
# that duration reflects network/OS latency, not this repo's performance, and
# balloons an individual test from milliseconds to tens of seconds.
pytestmark = pytest.mark.no_bench

# A port that refuses connections, so the daemon check deterministically fails.
UNREACHABLE = "http://127.0.0.1:9"


class _FakeProjectsClient:
    """Stand-in for DaemonClient that serves list_projects() (and a canned
    health(), since it also replaces the client `_check_daemon` uses)."""

    def __init__(self, projects: list[dict[str, Any]]) -> None:
        self._projects = projects

    def __call__(self, *_args: object, **_kwargs: object) -> _FakeProjectsClient:
        return self

    def __enter__(self) -> _FakeProjectsClient:
        return self

    def __exit__(self, *_exc: object) -> None:
        pass

    def health(self) -> dict[str, Any]:
        return {"name": "ralphus-daemon", "version": "test", "warnings": []}

    def list_projects(self) -> dict[str, Any]:
        return {"projects": self._projects}


def _git_repo(tmp_path: Path) -> Path:
    repo = tmp_path / "repo"
    repo.mkdir()
    subprocess.run(["git", "init"], cwd=repo, capture_output=True, check=True)
    return repo


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


@pytest.mark.skipif(shutil.which("git") is None, reason="git not on PATH")
def test_project_with_valid_git_repo_path_passes(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    repo = _git_repo(tmp_path)
    fake = _FakeProjectsClient([{"name": "myproj", "path": str(repo)}])
    monkeypatch.setattr("ralphus.health.DaemonClient", fake)
    results = run_checks(UNREACHABLE)
    project = next(r for r in results if r.name == "project:myproj")
    assert project.status == "pass"
    assert not project.is_fail


def test_project_with_missing_path_fails(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    missing = tmp_path / "does-not-exist"
    fake = _FakeProjectsClient([{"name": "myproj", "path": str(missing)}])
    monkeypatch.setattr("ralphus.health.DaemonClient", fake)
    results = run_checks(UNREACHABLE)
    project = next(r for r in results if r.name == "project:myproj")
    assert project.is_fail
    assert "does not exist" in project.detail


def test_project_with_non_git_directory_fails(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    not_a_repo = tmp_path / "plain-dir"
    not_a_repo.mkdir()
    fake = _FakeProjectsClient([{"name": "myproj", "path": str(not_a_repo)}])
    monkeypatch.setattr("ralphus.health.DaemonClient", fake)
    results = run_checks(UNREACHABLE)
    project = next(r for r in results if r.name == "project:myproj")
    assert project.is_fail
    assert "not a git repository" in project.detail


@pytest.mark.skipif(shutil.which("git") is None, reason="git not on PATH")
def test_multiple_projects_each_get_their_own_check(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    good = _git_repo(tmp_path)
    fake = _FakeProjectsClient(
        [
            {"name": "good", "path": str(good)},
            {"name": "bad", "path": str(tmp_path / "missing")},
        ]
    )
    monkeypatch.setattr("ralphus.health.DaemonClient", fake)
    results = run_checks(UNREACHABLE)
    good_result = next(r for r in results if r.name == "project:good")
    bad_result = next(r for r in results if r.name == "project:bad")
    assert good_result.status == "pass"
    assert bad_result.is_fail


def test_no_registered_projects_adds_no_checks(monkeypatch: pytest.MonkeyPatch) -> None:
    fake = _FakeProjectsClient([])
    monkeypatch.setattr("ralphus.health.DaemonClient", fake)
    results = run_checks(UNREACHABLE)
    assert not any(r.name.startswith("project:") for r in results)


def test_daemon_unreachable_adds_no_project_checks() -> None:
    # DaemonClient isn't monkeypatched here, so listing projects hits the real
    # (refused) UNREACHABLE port and _check_projects must degrade silently --
    # `_check_daemon` already reports that failure.
    results = run_checks(UNREACHABLE)
    assert not any(r.name.startswith("project:") for r in results)


@pytest.mark.parametrize(
    ("value", "expected"),
    [
        ("claude", False),
        ("/usr/bin/claude", False),
        ('"C:\\Program Files\\claude\\claude.exe"', False),
        ("cd foo bar ; ./claude", True),
        ('"cd foo" ; ./claude', True),
        ("./claude --flag", True),
    ],
)
def test_is_compound_shell_command(value: str, expected: bool) -> None:
    assert is_compound_shell_command(value) is expected


def test_unquote_path_strips_matching_quotes() -> None:
    assert unquote_path('"C:\\claude\\claude.exe"') == "C:\\claude\\claude.exe"


def test_unquote_path_leaves_bare_path_alone() -> None:
    assert unquote_path("/usr/bin/claude") == "/usr/bin/claude"


def test_claude_command_not_set_is_a_pass(monkeypatch: pytest.MonkeyPatch) -> None:
    monkeypatch.delenv("RALPHUS_CLAUDE_COMMAND", raising=False)
    results = run_checks(UNREACHABLE)
    result = next(r for r in results if r.name == "claude-command")
    assert result.status == "pass"
    assert not result.is_fail


def test_claude_command_compound_skips_path_check(monkeypatch: pytest.MonkeyPatch) -> None:
    monkeypatch.setenv("RALPHUS_CLAUDE_COMMAND", "cd foo bar ; ./claude")
    results = run_checks(UNREACHABLE)
    result = next(r for r in results if r.name == "claude-command")
    assert result.status == "pass"
    assert "compound" in result.detail


def test_claude_command_missing_bare_path_fails(monkeypatch: pytest.MonkeyPatch) -> None:
    monkeypatch.setenv("RALPHUS_CLAUDE_COMMAND", "/no/such/claude-binary")
    results = run_checks(UNREACHABLE)
    result = next(r for r in results if r.name == "claude-command")
    assert result.is_fail


def test_claude_command_existing_file_passes(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    exe = tmp_path / "claude"
    exe.write_text("#!/bin/sh\n", encoding="utf-8")
    exe.chmod(0o755)
    monkeypatch.setenv("RALPHUS_CLAUDE_COMMAND", str(exe))
    results = run_checks(UNREACHABLE)
    result = next(r for r in results if r.name == "claude-command")
    assert result.status == "pass"


@pytest.mark.skipif(os.name == "nt", reason="POSIX execute bit isn't meaningful on Windows")
def test_claude_command_non_executable_file_fails(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    exe = tmp_path / "claude"
    exe.write_text("not executable", encoding="utf-8")
    exe.chmod(0o644)
    monkeypatch.setenv("RALPHUS_CLAUDE_COMMAND", str(exe))
    results = run_checks(UNREACHABLE)
    result = next(r for r in results if r.name == "claude-command")
    assert result.is_fail
    assert "not executable" in result.detail


def test_cli_check_health_reports_a_broken_project(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch, capsys: pytest.CaptureFixture[str]
) -> None:
    missing = tmp_path / "does-not-exist"
    fake = _FakeProjectsClient([{"name": "myproj", "path": str(missing)}])
    monkeypatch.setattr("ralphus.health.DaemonClient", fake)
    code = cli.main(["--daemon-url", UNREACHABLE, "check", "health"])
    out = capsys.readouterr().out
    assert code == 1
    assert "project:myproj" in out
    assert "FAIL" in out
