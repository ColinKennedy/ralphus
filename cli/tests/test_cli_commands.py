"""Tests for the ralphus CLI subcommands, with the daemon client faked out."""

from __future__ import annotations

import shutil
import subprocess
from pathlib import Path
from typing import Any, ClassVar

import pytest

import ralphus.__main__ as cli
from ralphus.client import ValidationOutcome


class _FakeClient:
    """Stand-in for DaemonClient that records calls and returns canned data."""

    last_submit: ClassVar[dict[str, Any]] = {}
    last_clear: ClassVar[dict[str, Any] | None] = None

    def __init__(self, *_args: object, **_kwargs: object) -> None:
        pass

    def __enter__(self) -> _FakeClient:
        return self

    def __exit__(self, *_exc: object) -> None:
        pass

    def validate(self, _text: str) -> ValidationOutcome:
        return ValidationOutcome(valid=True, errors=[], warnings=[])

    def submit(self, text: str, *, hold: bool = False, label: str | None = None) -> dict[str, Any]:
        _FakeClient.last_submit = {"text": text, "hold": hold, "label": label}
        return {"run_id": "run-000000000001", "state": "queued" if hold else "pending"}

    def run(self, run_id: str) -> dict[str, Any]:
        return {"id": run_id, "state": "done", "tasks": [{"name": "build", "state": "done"}]}

    def tasks(self) -> dict[str, Any]:
        return {"runs": [{"id": "run-000000000001", "state": "done", "label": "x"}]}

    def clear(
        self, *, states: list[str] | None = None, keep_temporary: bool = False
    ) -> dict[str, Any]:
        _FakeClient.last_clear = {"states": states, "keep_temporary": keep_temporary}
        return {"runs_deleted": 3, "guardians_deleted": 1, "worktrees_purged": 1}


@pytest.fixture(autouse=True)
def _patch_client(monkeypatch: pytest.MonkeyPatch) -> None:
    monkeypatch.setattr(cli, "DaemonClient", _FakeClient)
    # Force the API path for validate tests (no offline daemon binary).
    monkeypatch.setattr(cli, "_find_daemon_bin", lambda: None)


def _write(tmp_path: Path, body: str = "[[task]]\nname='t'\n") -> Path:
    path = tmp_path / "task.toml"
    path.write_text(body, encoding="utf-8")
    return path


def test_validate_ok(tmp_path: Path, capsys: pytest.CaptureFixture[str]) -> None:
    code = cli.main(["validate", str(_write(tmp_path))])
    assert code == 0
    assert "valid" in capsys.readouterr().out


def test_validate_missing_file(tmp_path: Path) -> None:
    code = cli.main(["validate", str(tmp_path / "nope.toml")])
    assert code == 2


class _FakeProc:
    def __init__(self, returncode: int) -> None:
        self.returncode = returncode


def test_validate_uses_offline_daemon_binary(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    # When a daemon binary is locatable, validate shells to it offline instead
    # of calling the API.
    monkeypatch.setattr(cli, "_find_daemon_bin", lambda: "ralphus-daemon")
    captured: dict[str, list[str]] = {}

    def fake_run(cmd: list[str], check: bool) -> _FakeProc:
        captured["cmd"] = cmd
        assert check is False
        return _FakeProc(0)

    monkeypatch.setattr(subprocess, "run", fake_run)
    code = cli.main(["validate", str(_write(tmp_path))])
    assert code == 0
    assert captured["cmd"][:2] == ["ralphus-daemon", "validate"]


def test_submit_prints_run_id(tmp_path: Path, capsys: pytest.CaptureFixture[str]) -> None:
    code = cli.main(["submit", str(_write(tmp_path)), "--label", "demo"])
    assert code == 0
    assert "run-000000000001" in capsys.readouterr().out
    assert _FakeClient.last_submit["label"] == "demo"


def test_submit_hold(tmp_path: Path, capsys: pytest.CaptureFixture[str]) -> None:
    code = cli.main(["submit", str(_write(tmp_path)), "--hold"])
    assert code == 0
    out = capsys.readouterr().out
    assert "queued" in out
    assert _FakeClient.last_submit["hold"] is True


def test_status_single_run(capsys: pytest.CaptureFixture[str]) -> None:
    code = cli.main(["status", "run-000000000001"])
    assert code == 0
    out = capsys.readouterr().out
    assert "run-000000000001" in out
    assert "build" in out


def test_status_all_runs(capsys: pytest.CaptureFixture[str]) -> None:
    code = cli.main(["status"])
    assert code == 0
    assert "run-000000000001" in capsys.readouterr().out


def test_clear_all_yes(capsys: pytest.CaptureFixture[str]) -> None:
    _FakeClient.last_clear = None
    code = cli.main(["clear", "--all", "--yes"])
    assert code == 0
    out = capsys.readouterr().out
    assert "cleared 3 run(s)" in out
    assert _FakeClient.last_clear == {"states": [], "keep_temporary": False}


def test_clear_keep_temporary_and_status_filter() -> None:
    _FakeClient.last_clear = None
    code = cli.main(["clear", "--status", "done,failed", "--keep-temporary", "--yes"])
    assert code == 0
    assert _FakeClient.last_clear == {
        "states": ["done", "failed"],
        "keep_temporary": True,
    }


def test_clear_rejects_unknown_status(capsys: pytest.CaptureFixture[str]) -> None:
    _FakeClient.last_clear = None
    code = cli.main(["clear", "--status", "done,bogus", "--yes"])
    assert code == 2
    assert "unknown status" in capsys.readouterr().err
    # Failed validation must not reach the daemon.
    assert _FakeClient.last_clear is None


def test_clear_requires_all_or_status(capsys: pytest.CaptureFixture[str]) -> None:
    code = cli.main(["clear", "--yes"])
    assert code == 2
    assert "pass --all" in capsys.readouterr().err


def test_clear_confirmation_abort(
    monkeypatch: pytest.MonkeyPatch, capsys: pytest.CaptureFixture[str]
) -> None:
    _FakeClient.last_clear = None
    monkeypatch.setattr(cli, "_prompt", lambda _msg: "n")
    code = cli.main(["clear", "--all"])
    assert code == 1
    assert "aborted" in capsys.readouterr().out
    assert _FakeClient.last_clear is None


def test_clear_confirmation_accept(monkeypatch: pytest.MonkeyPatch) -> None:
    _FakeClient.last_clear = None
    monkeypatch.setattr(cli, "_prompt", lambda _msg: "y")
    code = cli.main(["clear", "--all"])
    assert code == 0
    assert _FakeClient.last_clear == {"states": [], "keep_temporary": False}


def _git(cwd: Path, *args: str) -> str:
    return subprocess.run(
        ["git", *args],
        cwd=cwd,
        capture_output=True,
        text=True,
        check=True,
    ).stdout.strip()


@pytest.mark.skipif(shutil.which("git") is None, reason="git not on PATH")
def test_initialize_git_enables_rerere(tmp_path: Path, capsys: pytest.CaptureFixture[str]) -> None:
    _git(tmp_path, "init")
    code = cli.main(["initialize", "git", "--path", str(tmp_path)])
    assert code == 0
    out = capsys.readouterr().out
    assert "rerere.enabled = true" in out
    assert _git(tmp_path, "config", "rerere.enabled") == "true"
    assert _git(tmp_path, "config", "rerere.autoupdate") == "true"


@pytest.mark.skipif(shutil.which("git") is None, reason="git not on PATH")
def test_initialize_git_rejects_non_repo(
    tmp_path: Path, capsys: pytest.CaptureFixture[str]
) -> None:
    code = cli.main(["initialize", "git", "--path", str(tmp_path)])
    assert code == 2
    assert "not inside a git working tree" in capsys.readouterr().err
