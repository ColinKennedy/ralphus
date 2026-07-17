"""Tests for `ralphus history` and `ralphus listen` (RAL-140)."""

from __future__ import annotations

import time
from pathlib import Path
from typing import Any, ClassVar

import pytest

import ralphus.__main__ as cli
from ralphus.client import DaemonError


class _FakeClient:
    """Stand-in DaemonClient for history/listen tests.

    Class-level state (reset by the `_patch` fixture below) lets each test
    configure exactly the responses it needs; `pane_calls`/`run_call_count`
    let a test simulate a value that changes across successive polls.
    """

    run_state: ClassVar[str] = "running"
    session_state: ClassVar[str] = "running"
    verify_state: ClassVar[str] = "pending"
    guardian_status: ClassVar[str] = "in_review"
    branch_merge_status: ClassVar[str] = "pending"
    panes: ClassVar[list[dict[str, Any]]] = [{"active": True, "content": ""}]
    pane_calls: ClassVar[int] = 0
    # `None` means "never published" -- `ghost_get` raises a 404 `DaemonError`,
    # mirroring the real daemon (RAL-136's `GET /api/ghosts/{owner_uri}`).
    ghost_content: ClassVar[str | None] = None
    verify_output: ClassVar[str] = ""
    run_states_sequence: ClassVar[list[str] | None] = None
    run_call_count: ClassVar[int] = 0

    def __init__(self, *_args: object, **_kwargs: object) -> None:
        pass

    def __enter__(self) -> _FakeClient:
        return self

    def __exit__(self, *_exc: object) -> None:
        pass

    def run(self, run_id: str) -> dict[str, Any]:
        cls = _FakeClient
        if cls.run_states_sequence is not None:
            idx = min(cls.run_call_count, len(cls.run_states_sequence) - 1)
            state = cls.run_states_sequence[idx]
            cls.run_call_count += 1
        else:
            state = cls.run_state
        verify_step = {"state": cls.verify_state, "output": cls.verify_output}
        return {
            "id": run_id,
            "state": state,
            "tasks": [
                {
                    "name": "build",
                    "state": cls.session_state,
                    "verify": [verify_step],
                    "sessions": [
                        {
                            "id": "work",
                            "state": cls.session_state,
                            "verify": [verify_step],
                        }
                    ],
                }
            ],
        }

    def session_pane(self, *_args: object, **_kwargs: object) -> dict[str, Any]:
        cls = _FakeClient
        idx = min(cls.pane_calls, len(cls.panes) - 1)
        cls.pane_calls += 1
        return cls.panes[idx]

    def verify_pane(self, *_args: object, **_kwargs: object) -> dict[str, Any]:
        return self.session_pane()

    def ghost_get(self, owner_uri: str) -> dict[str, Any]:
        if _FakeClient.ghost_content is None:
            raise DaemonError("no such ghost", status_code=404)
        return {"owner_uri": owner_uri, "content": _FakeClient.ghost_content}

    def guardian_get(self, guardian_id: str) -> dict[str, Any]:
        return {
            "id": guardian_id,
            "status": _FakeClient.guardian_status,
            "branches": [
                {
                    "id": "branch-1",
                    "position": 0,
                    "branch": "feature/a",
                    "merge_status": _FakeClient.branch_merge_status,
                }
            ],
        }

    def guardian_list(self) -> list[dict[str, Any]]:
        return [{"id": "guardian-1", "name": "my-review"}]


@pytest.fixture(autouse=True)
def _patch(monkeypatch: pytest.MonkeyPatch, tmp_path: Path) -> None:
    monkeypatch.setattr(cli, "DaemonClient", _FakeClient)
    monkeypatch.setattr(cli, "_HISTORY_CURSOR_DIR", tmp_path / "cursors")
    monkeypatch.setattr(time, "sleep", lambda _seconds: None)
    _FakeClient.run_state = "running"
    _FakeClient.session_state = "running"
    _FakeClient.verify_state = "pending"
    _FakeClient.guardian_status = "in_review"
    _FakeClient.branch_merge_status = "pending"
    _FakeClient.panes = [{"active": True, "content": ""}]
    _FakeClient.pane_calls = 0
    _FakeClient.ghost_content = None
    _FakeClient.verify_output = ""
    _FakeClient.run_states_sequence = None
    _FakeClient.run_call_count = 0


# ── history ──────────────────────────────────────────────────────────────────


def test_history_snapshot_prints_active_pane_content(capsys: pytest.CaptureFixture[str]) -> None:
    _FakeClient.panes = [{"active": True, "content": "hello\n"}]
    code = cli.main(["history", "run-1/build/0"])
    assert code == 0
    assert capsys.readouterr().out == "hello\n"


def test_history_snapshot_prints_ghost_when_session_inactive(
    capsys: pytest.CaptureFixture[str],
) -> None:
    _FakeClient.panes = [{"active": False, "content": ""}]
    _FakeClient.ghost_content = "done\n"
    code = cli.main(["history", "run-1/build/0"])
    assert code == 0
    assert capsys.readouterr().out == "done\n"


def test_history_snapshot_no_history_recorded_yet(capsys: pytest.CaptureFixture[str]) -> None:
    _FakeClient.panes = [{"active": False, "content": ""}]
    _FakeClient.ghost_content = None
    code = cli.main(["history", "run-1/build/0"])
    assert code == 0
    assert "no history recorded" in capsys.readouterr().out


def test_history_snapshot_prints_verify_output_when_inactive(
    capsys: pytest.CaptureFixture[str],
) -> None:
    _FakeClient.panes = [{"active": False, "content": ""}]
    _FakeClient.verify_output = "verify output\n"
    code = cli.main(["history", "run-1/build/0/verify/0"])
    assert code == 0
    assert capsys.readouterr().out == "verify output\n"


def test_history_snapshot_verify_selector(capsys: pytest.CaptureFixture[str]) -> None:
    _FakeClient.panes = [{"active": True, "content": "verify output\n"}]
    code = cli.main(["history", "run-1/build/0/verify/0"])
    assert code == 0
    assert capsys.readouterr().out == "verify output\n"


def test_history_rejects_run_selector(capsys: pytest.CaptureFixture[str]) -> None:
    code = cli.main(["history", "run-1"])
    assert code == 2
    assert "session or verify" in capsys.readouterr().err


def test_history_rejects_task_selector(capsys: pytest.CaptureFixture[str]) -> None:
    code = cli.main(["history", "run-1/build"])
    assert code == 2
    assert "session or verify" in capsys.readouterr().err


def test_history_wait_until_valid_requires_live(capsys: pytest.CaptureFixture[str]) -> None:
    code = cli.main(["history", "run-1/build/0", "--wait-until-valid"])
    assert code == 2
    assert "--wait-until-valid requires --live" in capsys.readouterr().err


def test_history_live_fails_immediately_when_nothing_live(
    capsys: pytest.CaptureFixture[str],
) -> None:
    _FakeClient.panes = [{"active": False, "content": ""}]
    code = cli.main(["history", "run-1/build/0", "--live"])
    assert code == 1
    assert "nothing is currently live" in capsys.readouterr().err


def test_history_live_tails_then_prints_ghost_as_its_own_block(
    capsys: pytest.CaptureFixture[str],
) -> None:
    _FakeClient.panes = [
        {"active": True, "content": "line1\n"},
        {"active": True, "content": "line1\nline2\n"},
        {"active": False, "content": ""},
    ]
    _FakeClient.ghost_content = "agent finished the task\n"
    code = cli.main(["history", "run-1/build/0", "--live"])
    assert code == 0
    out = capsys.readouterr().out
    assert out.startswith("line1\nline2\n")
    assert "session ended; recorded note" in out
    assert "agent finished the task\n" in out


def test_history_live_tails_with_no_ghost_prints_no_extra_block(
    capsys: pytest.CaptureFixture[str],
) -> None:
    _FakeClient.panes = [
        {"active": True, "content": "line1\n"},
        {"active": False, "content": ""},
    ]
    _FakeClient.ghost_content = None
    code = cli.main(["history", "run-1/build/0", "--live"])
    assert code == 0
    assert capsys.readouterr().out == "line1\n"


def test_history_live_wait_until_valid_waits_then_tails(
    capsys: pytest.CaptureFixture[str],
) -> None:
    _FakeClient.panes = [
        {"active": False, "content": ""},
        {"active": False, "content": ""},
        {"active": True, "content": "started\n"},
        {"active": False, "content": ""},
    ]
    code = cli.main(["history", "run-1/build/0", "--live", "--wait-until-valid", "5"])
    assert code == 0
    assert "started\n" in capsys.readouterr().out


def test_history_live_wait_until_valid_times_out(capsys: pytest.CaptureFixture[str]) -> None:
    _FakeClient.panes = [{"active": False, "content": ""}]
    code = cli.main(["history", "run-1/build/0", "--live", "--wait-until-valid", "0.05"])
    assert code == 1
    assert "timed out" in capsys.readouterr().err


def test_history_live_cursor_survives_a_cli_process_restart(
    capsys: pytest.CaptureFixture[str], monkeypatch: pytest.MonkeyPatch
) -> None:
    """The #1 risk called out in RAL-140: a resumed CLI process must pick up
    the on-disk cursor and only print content beyond it, not replay
    everything from the start. Simulates "process restart mid-tail" by
    raising out of the sleep between polls (as a `KeyboardInterrupt` would
    kill a real process) after the first chunk was already saved, then
    invoking `cli.main` fresh -- with only the on-disk cursor file, no
    in-memory state -- against a pane that has grown further in the
    meantime.
    """
    _FakeClient.panes = [{"active": True, "content": "line1\n"}]
    monkeypatch.setattr(time, "sleep", lambda _seconds: (_ for _ in ()).throw(KeyboardInterrupt))

    with pytest.raises(KeyboardInterrupt):
        cli.main(["history", "run-1/build/0", "--live"])
    assert capsys.readouterr().out == "line1\n"

    _FakeClient.pane_calls = 0
    _FakeClient.panes = [
        {"active": True, "content": "line1\nline2\n"},
        {"active": False, "content": ""},
    ]
    monkeypatch.setattr(time, "sleep", lambda _seconds: None)

    code = cli.main(["history", "run-1/build/0", "--live"])
    assert code == 0
    assert capsys.readouterr().out == "line2\n"


def test_history_live_cursor_detects_discontinuity_and_reprints_from_scratch(
    capsys: pytest.CaptureFixture[str], monkeypatch: pytest.MonkeyPatch
) -> None:
    """If the tail no longer matches the saved cursor's fingerprint (e.g. the
    tmux session was torn down and recreated under the same deterministic
    name), nothing should be silently skipped -- the full current content is
    printed instead, with a stderr note explaining why.
    """
    _FakeClient.panes = [{"active": True, "content": "line1\n"}]
    monkeypatch.setattr(time, "sleep", lambda _seconds: (_ for _ in ()).throw(KeyboardInterrupt))

    with pytest.raises(KeyboardInterrupt):
        cli.main(["history", "run-1/build/0", "--live"])
    capsys.readouterr()

    _FakeClient.pane_calls = 0
    _FakeClient.panes = [
        {"active": True, "content": "fresh start\n"},
        {"active": False, "content": ""},
    ]
    monkeypatch.setattr(time, "sleep", lambda _seconds: None)

    code = cli.main(["history", "run-1/build/0", "--live"])
    assert code == 0
    out, err = capsys.readouterr()
    assert out.startswith("fresh start\n")
    assert "session restarted" in err


def test_history_live_json_mode_emits_ndjson_chunks(capsys: pytest.CaptureFixture[str]) -> None:
    _FakeClient.panes = [
        {"active": True, "content": "hi\n"},
        {"active": False, "content": ""},
    ]
    _FakeClient.ghost_content = "hi\n"
    code = cli.main(["--json", "history", "run-1/build/0", "--live"])
    assert code == 0
    out = capsys.readouterr().out
    assert '{"content": "hi\\n"}' in out
    assert '"event": "ended"' in out


# ── listen ───────────────────────────────────────────────────────────────────


def test_listen_returns_immediately_when_already_matching(
    capsys: pytest.CaptureFixture[str],
) -> None:
    _FakeClient.run_state = "done"
    code = cli.main(["listen", "run-1", "--until", "done"])
    assert code == 0
    assert "done" in capsys.readouterr().out


def test_listen_invalid_until_value_for_kind(capsys: pytest.CaptureFixture[str]) -> None:
    code = cli.main(["listen", "run-1", "--until", "bogus"])
    assert code == 2
    assert "not a valid status" in capsys.readouterr().err


def test_listen_polls_until_state_changes(capsys: pytest.CaptureFixture[str]) -> None:
    _FakeClient.run_states_sequence = ["pending", "pending", "running", "done"]
    code = cli.main(["listen", "run-1", "--until", "done"])
    assert code == 0
    assert "done" in capsys.readouterr().out


def test_listen_times_out(capsys: pytest.CaptureFixture[str]) -> None:
    _FakeClient.run_states_sequence = ["pending"] * 1000
    code = cli.main(["listen", "run-1", "--until", "done", "--timeout", "0.05"])
    assert code == 1
    assert "timed out" in capsys.readouterr().err


def test_listen_session_selector(capsys: pytest.CaptureFixture[str]) -> None:
    _FakeClient.session_state = "done"
    code = cli.main(["listen", "run-1/build/0", "--until", "done"])
    assert code == 0


def test_listen_verify_selector(capsys: pytest.CaptureFixture[str]) -> None:
    _FakeClient.verify_state = "failed"
    code = cli.main(["listen", "run-1/build/0/verify/0", "--until", "failed"])
    assert code == 0


def test_listen_review_selector(capsys: pytest.CaptureFixture[str]) -> None:
    _FakeClient.guardian_status = "approved"
    code = cli.main(["listen", "guardian-1", "--until", "approved"])
    assert code == 0


def test_listen_review_worktree_selector(capsys: pytest.CaptureFixture[str]) -> None:
    _FakeClient.branch_merge_status = "done"
    code = cli.main(["listen", "guardian-1#0", "--until", "done"])
    assert code == 0
