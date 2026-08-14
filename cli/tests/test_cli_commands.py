"""Tests for the ralphus CLI subcommands, with the daemon client faked out."""

from __future__ import annotations

import shutil
import subprocess
import sys
from pathlib import Path
from typing import Any, ClassVar

import pytest

import ralphus.__main__ as cli
from ralphus.client import DaemonError, ValidationOutcome


class _FakeClient:
    """Stand-in for DaemonClient that records calls and returns canned data."""

    last_submit: ClassVar[dict[str, Any]] = {}
    last_clear: ClassVar[dict[str, Any] | None] = None
    submissions: ClassVar[list[dict[str, Any]]] = []
    activated: ClassVar[list[str]] = []
    run_states: ClassVar[dict[str, str]] = {}
    next_run_seq: ClassVar[list[int]] = [1]
    last_register_project: ClassVar[dict[str, Any] | None] = None
    projects: ClassVar[list[dict[str, Any]]] = []
    retried_runs: ClassVar[list[str]] = []
    restarted_tasks: ClassVar[list[tuple[str, int]]] = []
    restarted_sessions: ClassVar[list[tuple[str, int, int]]] = []
    restarted_session_verifies: ClassVar[list[tuple[str, int, int, int]]] = []
    restarted_task_verifies: ClassVar[list[tuple[str, int, int]]] = []
    env_calls: ClassVar[list[dict[str, Any]]] = []
    task_env_calls: ClassVar[list[dict[str, Any]]] = []
    task_verify_env_calls: ClassVar[list[dict[str, Any]]] = []
    session_env_calls: ClassVar[list[dict[str, Any]]] = []
    session_verify_env_calls: ClassVar[list[dict[str, Any]]] = []

    def __init__(self, *_args: object, **_kwargs: object) -> None:
        pass

    def __enter__(self) -> _FakeClient:
        return self

    def __exit__(self, *_exc: object) -> None:
        pass

    def validate(self, _text: str) -> ValidationOutcome:
        return ValidationOutcome(valid=True, errors=[], warnings=[])

    def submit(self, text: str, *, hold: bool = False, label: str | None = None) -> dict[str, Any]:
        seq = _FakeClient.next_run_seq[0]
        _FakeClient.next_run_seq[0] += 1
        run_id = f"run-{seq:012d}"
        state = "queued" if hold else "pending"
        entry = {"text": text, "hold": hold, "label": label, "run_id": run_id}
        _FakeClient.last_submit = entry
        _FakeClient.submissions.append(entry)
        _FakeClient.run_states[run_id] = state
        return {"run_id": run_id, "state": state}

    def activate_run(self, run_id: str) -> dict[str, Any]:
        _FakeClient.activated.append(run_id)
        _FakeClient.run_states[run_id] = "pending"
        return {"run_id": run_id, "state": "pending"}

    def run(self, run_id: str) -> dict[str, Any]:
        state = _FakeClient.run_states.get(run_id, "done")
        return {
            "id": run_id,
            "state": state,
            "tasks": [
                {
                    "name": "build",
                    "state": "done",
                    "sessions": [{"id": "s0", "name": "compile", "verify": [{}, {}]}],
                    "verify": [{}],
                }
            ],
        }

    def retry_run(self, run_id: str) -> dict[str, Any]:
        _FakeClient.retried_runs.append(run_id)
        return {"state": "pending"}

    def restart_task(self, run_id: str, task_idx: int) -> dict[str, Any]:
        _FakeClient.restarted_tasks.append((run_id, task_idx))
        return {"state": "pending", "dirtied": []}

    def restart_session(self, run_id: str, task_idx: int, session_idx: int) -> dict[str, Any]:
        _FakeClient.restarted_sessions.append((run_id, task_idx, session_idx))
        return {"state": "pending", "dirtied": []}

    def restart_session_verify(
        self, run_id: str, task_idx: int, session_idx: int, verify_idx: int
    ) -> dict[str, Any]:
        _FakeClient.restarted_session_verifies.append((run_id, task_idx, session_idx, verify_idx))
        return {"state": "pending"}

    def restart_task_verify(self, run_id: str, task_idx: int, verify_idx: int) -> dict[str, Any]:
        _FakeClient.restarted_task_verifies.append((run_id, task_idx, verify_idx))
        return {"state": "pending"}

    def set_run_env(
        self,
        run_id: str,
        *,
        set_vars: dict[str, str] | None = None,
        unset_vars: list[str] | None = None,
    ) -> dict[str, str]:
        _FakeClient.env_calls.append(
            {"run_id": run_id, "set": set_vars or {}, "unset": unset_vars or []}
        )
        return set_vars or {}

    def set_task_env(
        self,
        run_id: str,
        task_idx: int,
        *,
        set_vars: dict[str, str] | None = None,
        unset_vars: list[str] | None = None,
    ) -> dict[str, str]:
        _FakeClient.task_env_calls.append(
            {
                "run_id": run_id,
                "task_idx": task_idx,
                "set": set_vars or {},
                "unset": unset_vars or [],
            }
        )
        return set_vars or {}

    def set_task_verify_env(
        self,
        run_id: str,
        task_idx: int,
        *,
        set_vars: dict[str, str] | None = None,
        unset_vars: list[str] | None = None,
    ) -> dict[str, str]:
        _FakeClient.task_verify_env_calls.append(
            {
                "run_id": run_id,
                "task_idx": task_idx,
                "set": set_vars or {},
                "unset": unset_vars or [],
            }
        )
        return set_vars or {}

    def set_session_env(
        self,
        run_id: str,
        task_idx: int,
        session_idx: int,
        *,
        set_vars: dict[str, str] | None = None,
        unset_vars: list[str] | None = None,
    ) -> dict[str, str]:
        _FakeClient.session_env_calls.append(
            {
                "run_id": run_id,
                "task_idx": task_idx,
                "session_idx": session_idx,
                "set": set_vars or {},
                "unset": unset_vars or [],
            }
        )
        return set_vars or {}

    def set_session_verify_env(
        self,
        run_id: str,
        task_idx: int,
        session_idx: int,
        *,
        set_vars: dict[str, str] | None = None,
        unset_vars: list[str] | None = None,
    ) -> dict[str, str]:
        _FakeClient.session_verify_env_calls.append(
            {
                "run_id": run_id,
                "task_idx": task_idx,
                "session_idx": session_idx,
                "set": set_vars or {},
                "unset": unset_vars or [],
            }
        )
        return set_vars or {}

    def tasks(self) -> dict[str, Any]:
        return {"runs": [{"id": "run-000000000001", "state": "done", "label": "x"}]}

    def clear(
        self, *, states: list[str] | None = None, keep_temporary: bool = False
    ) -> dict[str, Any]:
        _FakeClient.last_clear = {"states": states, "keep_temporary": keep_temporary}
        return {"runs_deleted": 3, "guardians_deleted": 1, "worktrees_purged": 1}

    def guardian_get(self, guardian_id: str) -> dict[str, Any]:
        return {
            "id": guardian_id,
            "name": "my-review",
            "status": "in_review",
            "branches": [
                {
                    "id": "branch-000000000001",
                    "position": 0,
                    "branch": "feature/a",
                    "merge_status": "done",
                }
            ],
        }

    def guardian_list(self) -> list[dict[str, Any]]:
        return [{"id": "guardian-000000000001", "name": "my-review"}]

    def register_project(
        self, name: str, path: str, *, description: str = "", vcs: str = "git"
    ) -> dict[str, Any]:
        _FakeClient.last_register_project = {
            "name": name,
            "path": path,
            "description": description,
            "vcs": vcs,
        }
        return {"name": name}

    def list_projects(self) -> dict[str, Any]:
        return {"projects": _FakeClient.projects}

    def get_project(self, name: str) -> dict[str, Any]:
        for p in _FakeClient.projects:
            if p["name"] == name:
                return p
        raise DaemonError(f'project "{name}" is not registered')


@pytest.fixture(autouse=True)
def _patch_client(monkeypatch: pytest.MonkeyPatch) -> None:
    monkeypatch.setattr(cli, "DaemonClient", _FakeClient)
    # Force the API path for validate tests (no offline daemon binary).
    monkeypatch.setattr(cli, "_find_daemon_bin", lambda: None)
    _FakeClient.submissions = []
    _FakeClient.activated = []
    _FakeClient.run_states = {}
    _FakeClient.next_run_seq = [1]
    _FakeClient.retried_runs = []
    _FakeClient.restarted_tasks = []
    _FakeClient.restarted_sessions = []
    _FakeClient.restarted_session_verifies = []
    _FakeClient.restarted_task_verifies = []
    _FakeClient.env_calls = []
    _FakeClient.task_env_calls = []
    _FakeClient.task_verify_env_calls = []
    _FakeClient.session_env_calls = []
    _FakeClient.session_verify_env_calls = []


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


def test_validate_offline_single_file_uses_real_path(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    # A single-file offline validation shells out with the real on-disk path
    # (not a scratch copy), so any reported line numbers stay meaningful.
    monkeypatch.setattr(cli, "_find_daemon_bin", lambda: "ralphus-daemon")
    path = _write(tmp_path)
    captured: dict[str, list[str]] = {}

    def fake_run(cmd: list[str], check: bool) -> _FakeProc:
        captured["cmd"] = cmd
        assert check is False
        return _FakeProc(0)

    monkeypatch.setattr(subprocess, "run", fake_run)
    code = cli.main(["validate", str(path)])
    assert code == 0
    assert captured["cmd"] == ["ralphus-daemon", "validate", str(path)]


def test_validate_multiple_files_combines_offline(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    # Multiple files combine into one scratch document for the offline
    # daemon-binary path, mirroring how `submit` combines them into one call.
    monkeypatch.setattr(cli, "_find_daemon_bin", lambda: "ralphus-daemon")
    path_a = tmp_path / "a.toml"
    path_a.write_text("[[task]]\nname='a'\n", encoding="utf-8")
    path_b = tmp_path / "b.toml"
    path_b.write_text("[[task]]\nname='b'\n", encoding="utf-8")
    captured: dict[str, list[str]] = {}

    def fake_run(cmd: list[str], check: bool) -> _FakeProc:
        captured["cmd"] = cmd
        assert check is False
        # The scratch file exists and contains both files' contents combined.
        scratch = Path(cmd[2])
        text = scratch.read_text(encoding="utf-8")
        assert "name='a'" in text
        assert "name='b'" in text
        return _FakeProc(0)

    monkeypatch.setattr(subprocess, "run", fake_run)
    code = cli.main(["validate", str(path_a), str(path_b)])
    assert code == 0
    assert captured["cmd"][:2] == ["ralphus-daemon", "validate"]
    # The scratch file used for validation is cleaned up afterward.
    assert not Path(captured["cmd"][2]).exists()


def test_validate_multiple_files_combines_via_api(
    tmp_path: Path, capsys: pytest.CaptureFixture[str]
) -> None:
    # With no offline daemon binary available (the default in this test
    # module), multiple files still combine into ONE validate() call.
    path_a = tmp_path / "a.toml"
    path_a.write_text("[[task]]\nname='a'\n", encoding="utf-8")
    path_b = tmp_path / "b.toml"
    path_b.write_text("[[task]]\nname='b'\n", encoding="utf-8")

    code = cli.main(["validate", str(path_a), str(path_b)])
    assert code == 0
    assert "valid" in capsys.readouterr().out


def test_validate_missing_file_in_multi_file_list(tmp_path: Path) -> None:
    code = cli.main(["validate", str(_write(tmp_path)), str(tmp_path / "nope.toml")])
    assert code == 2


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


def test_submit_multiple_files_combines_into_one_call(
    tmp_path: Path, capsys: pytest.CaptureFixture[str]
) -> None:
    # Multiple files must become ONE submission (one client.submit() call) so a
    # shared `ralphus:new-review/<key>` folds into a single review instead of
    # minting one guardian per file.
    path_a = tmp_path / "a.toml"
    path_a.write_text("[[task]]\nname='a'\n", encoding="utf-8")
    path_b = tmp_path / "b.toml"
    path_b.write_text("[[task]]\nname='b'\n", encoding="utf-8")
    path_c = tmp_path / "c.toml"
    path_c.write_text("[[task]]\nname='c'\n", encoding="utf-8")

    code = cli.main(["submit", str(path_a), str(path_b), str(path_c)])
    assert code == 0
    assert "run-000000000001" in capsys.readouterr().out

    combined = _FakeClient.last_submit["text"]
    assert "name='a'" in combined
    assert "name='b'" in combined
    assert "name='c'" in combined


def test_submit_missing_file_in_multi_file_list(tmp_path: Path) -> None:
    code = cli.main(["submit", str(_write(tmp_path)), str(tmp_path / "nope.toml")])
    assert code == 2


# ── Q6 (CLI_PARITY_PLAN.local.md): submit/author parity ──────────────────────


def test_submit_from_stdin(
    monkeypatch: pytest.MonkeyPatch, capsys: pytest.CaptureFixture[str]
) -> None:
    import io

    monkeypatch.setattr(sys, "stdin", io.StringIO("[[task]]\nname='from-stdin'\n"))
    code = cli.main(["submit", "-"])
    assert code == 0
    assert "run-000000000001" in capsys.readouterr().out
    assert "from-stdin" in _FakeClient.last_submit["text"]


def test_submit_stdin_empty_is_an_error(monkeypatch: pytest.MonkeyPatch) -> None:
    import io

    monkeypatch.setattr(sys, "stdin", io.StringIO("   "))
    code = cli.main(["submit", "-"])
    assert code == 2


def test_submit_directory_submits_each_file_as_its_own_run(
    tmp_path: Path, capsys: pytest.CaptureFixture[str]
) -> None:
    (tmp_path / "a.toml").write_text("[[task]]\nname='a'\n", encoding="utf-8")
    (tmp_path / "b.toml").write_text("[[task]]\nname='b'\n", encoding="utf-8")
    (tmp_path / "not-toml.txt").write_text("ignore me", encoding="utf-8")

    code = cli.main(["submit", str(tmp_path)])
    assert code == 0
    assert len(_FakeClient.submissions) == 2
    texts = [s["text"] for s in _FakeClient.submissions]
    assert any("name='a'" in t for t in texts)
    assert any("name='b'" in t for t in texts)
    # Each submission is separate -- neither combines the other file's content.
    assert not any("name='a'" in t and "name='b'" in t for t in texts)


def test_submit_glob_submits_each_match_as_its_own_run(tmp_path: Path) -> None:
    (tmp_path / "x.toml").write_text("[[task]]\nname='x'\n", encoding="utf-8")
    (tmp_path / "y.toml").write_text("[[task]]\nname='y'\n", encoding="utf-8")

    code = cli.main(["submit", str(tmp_path / "*.toml")])
    assert code == 0
    assert len(_FakeClient.submissions) == 2


def test_submit_empty_glob_is_an_error(tmp_path: Path) -> None:
    code = cli.main(["submit", str(tmp_path / "*.toml")])
    assert code == 2


def test_submit_empty_directory_is_an_error(tmp_path: Path) -> None:
    code = cli.main(["submit", str(tmp_path)])
    assert code == 2


def test_submit_activate_forces_hold_then_activates(tmp_path: Path) -> None:
    code = cli.main(["submit", str(_write(tmp_path)), "--activate"])
    assert code == 0
    assert _FakeClient.last_submit["hold"] is True
    assert _FakeClient.activated == ["run-000000000001"]


def test_submit_wait_reports_done_as_success(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch, capsys: pytest.CaptureFixture[str]
) -> None:
    # The base fake reports the state `submit()` set (pending, for a non-hold
    # submission), which never changes -- `_wait_for_terminal` would poll
    # forever. Report "done" on the very first poll instead, so the test can't
    # hang on a real `time.sleep` if the polling logic ever regresses.
    class _DoneClient(_FakeClient):
        def run(self, run_id: str) -> dict[str, Any]:
            return {"id": run_id, "state": "done"}

    monkeypatch.setattr(cli, "DaemonClient", _DoneClient)
    code = cli.main(["submit", str(_write(tmp_path)), "--wait"])
    assert code == 0
    assert "run-000000000001: done" in capsys.readouterr().out


def test_submit_wait_reports_failed_as_error(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    class _FailingSubmitClient(_FakeClient):
        def submit(
            self, text: str, *, hold: bool = False, label: str | None = None
        ) -> dict[str, Any]:
            result = super().submit(text, hold=hold, label=label)
            _FakeClient.run_states[result["run_id"]] = "failed"
            return result

    monkeypatch.setattr(cli, "DaemonClient", _FailingSubmitClient)
    code = cli.main(["submit", str(_write(tmp_path)), "--wait"])
    assert code == 1


def test_submit_validates_before_submitting_and_blocks_on_invalid_toml(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch, capsys: pytest.CaptureFixture[str]
) -> None:
    class _InvalidClient(_FakeClient):
        def validate(self, _text: str) -> ValidationOutcome:
            return ValidationOutcome(
                valid=False, errors=[{"line": 3, "message": "missing cwd"}], warnings=[]
            )

    monkeypatch.setattr(cli, "DaemonClient", _InvalidClient)
    code = cli.main(["submit", str(_write(tmp_path))])
    assert code == 1
    assert "error [line 3]: missing cwd" in capsys.readouterr().err
    assert not _FakeClient.submissions


def test_submit_no_validate_skips_the_validate_pass_even_when_invalid(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    class _InvalidClient(_FakeClient):
        def validate(self, _text: str) -> ValidationOutcome:
            raise AssertionError("validate() must not be called with --no-validate")

    monkeypatch.setattr(cli, "DaemonClient", _InvalidClient)
    code = cli.main(["submit", str(_write(tmp_path)), "--no-validate"])
    assert code == 0
    assert _FakeClient.submissions


def test_submit_batch_validates_each_file_and_stops_at_the_first_invalid_one(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch, capsys: pytest.CaptureFixture[str]
) -> None:
    class _SecondFileInvalidClient(_FakeClient):
        def validate(self, text: str) -> ValidationOutcome:
            if "bad" in text:
                return ValidationOutcome(
                    valid=False, errors=[{"line": 1, "message": "bad task"}], warnings=[]
                )
            return ValidationOutcome(valid=True, errors=[], warnings=[])

    monkeypatch.setattr(cli, "DaemonClient", _SecondFileInvalidClient)
    (tmp_path / "a-good.toml").write_text("[[task]]\nname='good'\n", encoding="utf-8")
    (tmp_path / "b-bad.toml").write_text("[[task]]\nname='bad'\n", encoding="utf-8")

    code = cli.main(["submit", str(tmp_path)])
    assert code == 1
    assert "error [line 1]: bad task" in capsys.readouterr().err
    # The batch loop validates file-by-file: `a-good.toml` (sorted first)
    # submits before `b-bad.toml` is reached and blocks the rest of the batch.
    assert len(_FakeClient.submissions) == 1
    assert "name='good'" in _FakeClient.last_submit["text"]


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


# ── Phase 7 (CLI_PARITY_PLAN.local.md): `ralphus get` ─────────────────────────


def test_get_run_whole_object_is_json(capsys: pytest.CaptureFixture[str]) -> None:
    code = cli.main(["get", "run-000000000001"])
    assert code == 0
    out = capsys.readouterr().out
    assert '"id": "run-000000000001"' in out


def test_get_run_field(capsys: pytest.CaptureFixture[str]) -> None:
    code = cli.main(["get", "run-000000000001", "state"])
    assert code == 0
    assert capsys.readouterr().out.strip() == "done"


def test_get_task_field_by_dotted_index(capsys: pytest.CaptureFixture[str]) -> None:
    code = cli.main(["get", "run-000000000001/0", "state"])
    assert code == 0
    assert capsys.readouterr().out.strip() == "done"


def test_get_unknown_field_is_an_error(capsys: pytest.CaptureFixture[str]) -> None:
    code = cli.main(["get", "run-000000000001", "no_such_field"])
    assert code == 2
    assert "no field" in capsys.readouterr().err


def test_get_guardian_field(capsys: pytest.CaptureFixture[str]) -> None:
    code = cli.main(["get", "guardian-000000000001", "status"])
    assert code == 0
    assert capsys.readouterr().out.strip() == "in_review"


def test_get_guardian_branch_field(capsys: pytest.CaptureFixture[str]) -> None:
    code = cli.main(["get", "guardian-000000000001#0", "merge_status"])
    assert code == 0
    assert capsys.readouterr().out.strip() == "done"


def test_get_guardian_by_name(capsys: pytest.CaptureFixture[str]) -> None:
    code = cli.main(["get", "@my-review", "name"])
    assert code == 0
    assert capsys.readouterr().out.strip() == "my-review"


def test_project_git_posts_resolved_path(
    tmp_path: Path, capsys: pytest.CaptureFixture[str]
) -> None:
    _FakeClient.last_register_project = None
    code = cli.main(
        [
            "project",
            "git",
            "--path",
            str(tmp_path),
            "--name",
            "my-project",
            "--description",
            "the project",
        ]
    )
    assert code == 0
    assert _FakeClient.last_register_project == {
        "name": "my-project",
        "path": str(tmp_path.resolve()),
        "description": "the project",
        "vcs": "git",
    }
    assert "my-project" in capsys.readouterr().out


def test_project_git_defaults_description_empty(tmp_path: Path) -> None:
    _FakeClient.last_register_project = None
    code = cli.main(["project", "git", "--path", str(tmp_path), "--name", "my-project"])
    assert code == 0
    assert _FakeClient.last_register_project is not None
    assert _FakeClient.last_register_project["description"] == ""


def test_project_list_prints_registered_projects_with_description(
    capsys: pytest.CaptureFixture[str],
) -> None:
    _FakeClient.projects = [
        {"name": "ralphus", "description": "the ralphus repo", "path": "C:/repo", "vcs": "git"},
        {"name": "other", "description": "", "path": "C:/other", "vcs": "git"},
    ]
    code = cli.main(["project", "list"])
    assert code == 0
    lines = capsys.readouterr().out.splitlines()
    assert lines[0].startswith("ralphus")
    assert "C:/repo" in lines[0]
    assert lines[1] == "    the ralphus repo"
    assert lines[2].startswith("other")
    assert "C:/other" in lines[2]
    # No description for "other" -> no indented line follows it.
    assert len(lines) == 3


def test_elide_right_leaves_short_text_untouched() -> None:
    assert cli._elide_right("short", 80) == "short"
    assert cli._elide_right("x" * 80, 80) == "x" * 80


def test_elide_right_truncates_and_appends_ellipsis() -> None:
    result = cli._elide_right("x" * 200, 80)
    assert len(result) == 80
    assert result == "x" * 77 + "..."


def test_project_list_short_elides_long_description(capsys: pytest.CaptureFixture[str]) -> None:
    long_description = "x" * 200
    _FakeClient.projects = [
        {"name": "ralphus", "description": long_description, "path": "C:/repo", "vcs": "git"},
    ]
    code = cli.main(["project", "list", "--short"])
    assert code == 0
    lines = capsys.readouterr().out.splitlines()
    assert lines[1] == "    " + "x" * 77 + "..."
    assert len(lines[1]) - 4 == cli._SHORT_DESCRIPTION_MAX


def test_project_list_short_leaves_short_description_untouched(
    capsys: pytest.CaptureFixture[str],
) -> None:
    _FakeClient.projects = [
        {"name": "ralphus", "description": "a short one", "path": "C:/repo", "vcs": "git"},
    ]
    code = cli.main(["project", "list", "--short"])
    assert code == 0
    lines = capsys.readouterr().out.splitlines()
    assert lines[1] == "    a short one"


def test_project_list_without_short_prints_full_long_description(
    capsys: pytest.CaptureFixture[str],
) -> None:
    long_description = "x" * 200
    _FakeClient.projects = [
        {"name": "ralphus", "description": long_description, "path": "C:/repo", "vcs": "git"},
    ]
    code = cli.main(["project", "list"])
    assert code == 0
    lines = capsys.readouterr().out.splitlines()
    assert lines[1] == "    " + long_description


def test_project_list_empty(capsys: pytest.CaptureFixture[str]) -> None:
    _FakeClient.projects = []
    code = cli.main(["project", "list"])
    assert code == 0
    assert "no registered projects" in capsys.readouterr().out


def test_project_get_prints_details(capsys: pytest.CaptureFixture[str]) -> None:
    _FakeClient.projects = [
        {"name": "ralphus", "description": "the ralphus repo", "path": "C:/repo", "vcs": "git"},
    ]
    code = cli.main(["project", "get", "ralphus"])
    assert code == 0
    out = capsys.readouterr().out
    assert "name:        ralphus" in out
    assert "vcs:         git" in out
    assert "path:        C:/repo" in out
    assert "description: the ralphus repo" in out


def test_project_get_unknown_name_errors(capsys: pytest.CaptureFixture[str]) -> None:
    _FakeClient.projects = []
    code = cli.main(["project", "get", "nope"])
    assert code == 2
    assert "is not registered" in capsys.readouterr().err


def test_agent_list_shows_fixed_model_list_for_claude_code(
    capsys: pytest.CaptureFixture[str],
) -> None:
    code = cli.main(["agent", "list"])
    assert code == 0
    out = capsys.readouterr().out
    assert "claude-code" in out
    assert "sonnet, opus, haiku, fable" in out


def test_agent_list_shows_any_model_for_open_agents(capsys: pytest.CaptureFixture[str]) -> None:
    code = cli.main(["agent", "list"])
    assert code == 0
    out = capsys.readouterr().out
    ollama_line = next(line for line in out.splitlines() if line.startswith("ollama"))
    assert "<any model>" in ollama_line


def test_bare_agent_command_lists_agents(capsys: pytest.CaptureFixture[str]) -> None:
    code = cli.main(["agent"])
    assert code == 0
    assert "claude-code" in capsys.readouterr().out


# ── retry (RAL-150) ────────────────────────────────────────────────────────


def test_retry_run_selector_dispatches_to_retry_run(capsys: pytest.CaptureFixture[str]) -> None:
    code = cli.main(["retry", "run-000000000001"])
    assert code == 0
    assert _FakeClient.retried_runs == ["run-000000000001"]
    assert "run-000000000001 -> pending" in capsys.readouterr().out


def test_retry_task_selector_dispatches_to_restart_task() -> None:
    code = cli.main(["retry", "run-000000000001/build"])
    assert code == 0
    assert _FakeClient.restarted_tasks == [("run-000000000001", 0)]


def test_retry_session_selector_dispatches_to_restart_session() -> None:
    code = cli.main(["retry", "run-000000000001/build/compile"])
    assert code == 0
    assert _FakeClient.restarted_sessions == [("run-000000000001", 0, 0)]


def test_retry_task_verify_selector_dispatches_to_restart_task_verify() -> None:
    code = cli.main(["retry", "run-000000000001/build/verify/0"])
    assert code == 0
    assert _FakeClient.restarted_task_verifies == [("run-000000000001", 0, 0)]


def test_retry_session_verify_selector_dispatches_to_restart_session_verify() -> None:
    code = cli.main(["retry", "run-000000000001/build/compile/verify/1"])
    assert code == 0
    assert _FakeClient.restarted_session_verifies == [("run-000000000001", 0, 0, 1)]


def test_retry_unknown_selector_is_an_error(capsys: pytest.CaptureFixture[str]) -> None:
    code = cli.main(["retry", "run-000000000001/nonexistent-task"])
    assert code == 2
    assert _FakeClient.restarted_tasks == []
    assert "no task named" in capsys.readouterr().err


def test_retry_with_environment_flag_sets_override_before_retrying() -> None:
    code = cli.main(["retry", "run-000000000001", "--environment", "A=1", "--environment", "B=2"])
    assert code == 0
    assert _FakeClient.env_calls == [
        {"run_id": "run-000000000001", "set": {"A": "1", "B": "2"}, "unset": []}
    ]
    assert _FakeClient.retried_runs == ["run-000000000001"]


def test_retry_with_unset_environment_flag() -> None:
    code = cli.main(
        ["retry", "run-000000000001", "--unset-environment", "A", "--unset-environment", "B"]
    )
    assert code == 0
    assert _FakeClient.env_calls == [{"run_id": "run-000000000001", "set": {}, "unset": ["A", "B"]}]


def test_retry_environment_flag_overrides_env_file_on_conflict(tmp_path: Path) -> None:
    env_file = tmp_path / "vars.env"
    env_file.write_text("A=from-file\nB=also-from-file\n", encoding="utf-8")
    code = cli.main(
        ["retry", "run-000000000001", "--env-file", str(env_file), "--environment", "A=from-flag"]
    )
    assert code == 0
    assert _FakeClient.env_calls == [
        {
            "run_id": "run-000000000001",
            "set": {"A": "from-flag", "B": "also-from-file"},
            "unset": [],
        }
    ]


def test_retry_env_file_supports_comments_blanks_and_quotes(tmp_path: Path) -> None:
    env_file = tmp_path / "vars.env"
    env_file.write_text(
        "# a comment\n\nKEY1=plain\nKEY2='single quoted'\nKEY3=\"double quoted\"\n",
        encoding="utf-8",
    )
    code = cli.main(["retry", "run-000000000001", "--env-file", str(env_file)])
    assert code == 0
    assert _FakeClient.env_calls == [
        {
            "run_id": "run-000000000001",
            "set": {"KEY1": "plain", "KEY2": "single quoted", "KEY3": "double quoted"},
            "unset": [],
        }
    ]


def test_retry_env_file_missing_file_is_an_error(
    tmp_path: Path, capsys: pytest.CaptureFixture[str]
) -> None:
    code = cli.main(["retry", "run-000000000001", "--env-file", str(tmp_path / "nope.env")])
    assert code == 2
    assert "could not read" in capsys.readouterr().err
    assert _FakeClient.env_calls == []


def test_retry_environment_flag_malformed_is_an_error(capsys: pytest.CaptureFixture[str]) -> None:
    code = cli.main(["retry", "run-000000000001", "--environment", "NOEQUALSSIGN"])
    assert code == 2
    assert "expects KEY=VAL" in capsys.readouterr().err
    assert _FakeClient.env_calls == []
    assert _FakeClient.retried_runs == []


def test_retry_without_environment_flags_does_not_call_set_run_env() -> None:
    code = cli.main(["retry", "run-000000000001"])
    assert code == 0
    assert _FakeClient.env_calls == []


# ── hierarchical env overrides (RAL-150 extension): --environment targets
# the level the selector itself names, not always the run ─────────────────


def test_retry_task_selector_with_environment_sets_task_env_not_run_env() -> None:
    code = cli.main(["retry", "run-000000000001/build", "--environment", "A=1"])
    assert code == 0
    assert _FakeClient.task_env_calls == [
        {"run_id": "run-000000000001", "task_idx": 0, "set": {"A": "1"}, "unset": []}
    ]
    assert _FakeClient.env_calls == []
    assert _FakeClient.restarted_tasks == [("run-000000000001", 0)]


def test_retry_session_selector_with_environment_sets_session_env() -> None:
    code = cli.main(["retry", "run-000000000001/build/compile", "--environment", "A=1"])
    assert code == 0
    assert _FakeClient.session_env_calls == [
        {
            "run_id": "run-000000000001",
            "task_idx": 0,
            "session_idx": 0,
            "set": {"A": "1"},
            "unset": [],
        }
    ]
    assert _FakeClient.env_calls == []
    assert _FakeClient.task_env_calls == []


def test_retry_task_verify_selector_with_environment_sets_task_verify_env() -> None:
    code = cli.main(["retry", "run-000000000001/build/verify/0", "--environment", "A=1"])
    assert code == 0
    assert _FakeClient.task_verify_env_calls == [
        {"run_id": "run-000000000001", "task_idx": 0, "set": {"A": "1"}, "unset": []}
    ]
    assert _FakeClient.env_calls == []
    assert _FakeClient.task_env_calls == []


def test_retry_session_verify_selector_with_environment_sets_session_verify_env() -> None:
    code = cli.main(["retry", "run-000000000001/build/compile/verify/1", "--environment", "A=1"])
    assert code == 0
    assert _FakeClient.session_verify_env_calls == [
        {
            "run_id": "run-000000000001",
            "task_idx": 0,
            "session_idx": 0,
            "set": {"A": "1"},
            "unset": [],
        }
    ]
    assert _FakeClient.env_calls == []
    assert _FakeClient.session_env_calls == []


def test_retry_run_selector_with_environment_still_sets_run_env() -> None:
    code = cli.main(["retry", "run-000000000001", "--environment", "A=1"])
    assert code == 0
    assert _FakeClient.env_calls == [{"run_id": "run-000000000001", "set": {"A": "1"}, "unset": []}]
    assert _FakeClient.task_env_calls == []


def test_retry_env_file_malformed_line_is_an_error(
    tmp_path: Path, capsys: pytest.CaptureFixture[str]
) -> None:
    env_file = tmp_path / "vars.env"
    env_file.write_text("NOEQUALSSIGN\n", encoding="utf-8")
    code = cli.main(["retry", "run-000000000001", "--env-file", str(env_file)])
    assert code == 2
    assert "expected KEY=VALUE" in capsys.readouterr().err
    assert _FakeClient.env_calls == []


def test_parse_env_file_strips_comments_blanks_and_quotes(tmp_path: Path) -> None:
    env_file = tmp_path / "vars.env"
    env_file.write_text(
        "# comment\n\n  \nA=1\nB='two'\nC=\"three\"\n",
        encoding="utf-8",
    )
    assert cli._parse_env_file(env_file) == {"A": "1", "B": "two", "C": "three"}


def test_parse_env_file_missing_file_raises() -> None:
    with pytest.raises(ValueError, match="could not read"):
        cli._parse_env_file(Path("does-not-exist.env"))


def test_parse_env_file_bad_line_raises(tmp_path: Path) -> None:
    env_file = tmp_path / "vars.env"
    env_file.write_text("NOTKEYVALUE\n", encoding="utf-8")
    with pytest.raises(ValueError, match="expected KEY=VALUE"):
        cli._parse_env_file(env_file)


def test_parse_environment_flags_last_duplicate_wins() -> None:
    assert cli._parse_environment_flags(["A=1", "A=2"]) == {"A": "2"}


def test_parse_environment_flags_bad_entry_raises() -> None:
    with pytest.raises(ValueError, match="expects KEY=VALUE"):
        cli._parse_environment_flags(["NOEQUALSSIGN"])
