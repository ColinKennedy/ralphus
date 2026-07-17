"""Tests for the run/task/session/verify and guardian/branch selector grammar."""

from __future__ import annotations

import httpx
import pytest

from ralphus.client import DaemonClient
from ralphus.selector import (
    RawRunSelector,
    ResolvedGuardianSelector,
    ResolvedSelector,
    SelectorError,
    parse_guardian_selector,
    parse_run_selector,
    resolve_guardian_selector,
    resolve_run_selector,
)

_RUN = {
    "id": "run-000000000001",
    "state": "running",
    "tasks": [
        {
            "name": "build",
            "state": "done",
            "sessions": [{"id": "s0", "name": "compile", "verify": [{}, {}]}],
            "verify": [{}],
        },
        {
            "name": "test",
            "state": "running",
            "sessions": [{"id": "s0", "name": None, "verify": []}],
            "verify": [],
        },
    ],
}

_GUARDIANS = [
    {"id": "guardian-000000000001", "name": "my-review"},
    {"id": "guardian-000000000002", "name": "other-review"},
]

_GUARDIAN_DETAIL = {
    "id": "guardian-000000000001",
    "name": "my-review",
    "branches": [
        {"id": "branch-000000000001", "position": 0, "branch": "feature/a"},
        {"id": "branch-000000000002", "position": 1, "branch": "feature/b"},
    ],
}


def _client_for(responses: dict[str, object]) -> DaemonClient:
    def handler(request: httpx.Request) -> httpx.Response:
        body = responses.get(request.url.path)
        assert body is not None, f"unexpected request to {request.url.path}"
        return httpx.Response(200, json=body)

    return DaemonClient("http://daemon.test", transport=httpx.MockTransport(handler))


# ── parse_run_selector (no network) ──────────────────────────────────────────


def test_parse_bare_run_id() -> None:
    assert parse_run_selector("run-1") == RawRunSelector("run-1")


def test_parse_task() -> None:
    assert parse_run_selector("run-1/build") == RawRunSelector("run-1", task="build")


def test_parse_session() -> None:
    assert parse_run_selector("run-1/build/compile") == RawRunSelector(
        "run-1", task="build", session="compile"
    )


def test_parse_task_verify() -> None:
    assert parse_run_selector("run-1/build/verify/0") == RawRunSelector(
        "run-1", task="build", verify_scope="task", verify="0"
    )


def test_parse_session_verify() -> None:
    assert parse_run_selector("run-1/build/compile/verify/1") == RawRunSelector(
        "run-1", task="build", session="compile", verify_scope="session", verify="1"
    )


def test_parse_verify_without_index_is_an_error() -> None:
    with pytest.raises(SelectorError):
        parse_run_selector("run-1/build/verify")


def test_parse_malformed_selector_is_an_error() -> None:
    with pytest.raises(SelectorError):
        parse_run_selector("run-1/build/compile/extra/segments/too/many")


def test_parse_empty_selector_is_an_error() -> None:
    with pytest.raises(SelectorError):
        parse_run_selector("")


# ── resolve_run_selector (name -> index, one fetch) ──────────────────────────


def test_resolve_bare_run_id_does_not_fetch() -> None:
    def handler(_request: httpx.Request) -> httpx.Response:
        raise AssertionError("should not fetch the run for a bare run selector")

    client = DaemonClient("http://daemon.test", transport=httpx.MockTransport(handler))
    assert resolve_run_selector(client, "run-1") == ResolvedSelector(kind="run", run_id="run-1")


def test_resolve_task_by_name() -> None:
    client = _client_for({"/api/runs/run-1": _RUN})
    assert resolve_run_selector(client, "run-1/build") == ResolvedSelector(
        kind="task", run_id="run-1", task_idx=0
    )


def test_resolve_task_by_index() -> None:
    client = _client_for({"/api/runs/run-1": _RUN})
    assert resolve_run_selector(client, "run-1/1") == ResolvedSelector(
        kind="task", run_id="run-1", task_idx=1
    )


def test_resolve_unknown_task_name_raises() -> None:
    client = _client_for({"/api/runs/run-1": _RUN})
    with pytest.raises(SelectorError, match="no task named"):
        resolve_run_selector(client, "run-1/nonexistent")


def test_resolve_session_by_name() -> None:
    client = _client_for({"/api/runs/run-1": _RUN})
    assert resolve_run_selector(client, "run-1/build/compile") == ResolvedSelector(
        kind="session", run_id="run-1", task_idx=0, session_idx=0
    )


def test_resolve_task_verify() -> None:
    client = _client_for({"/api/runs/run-1": _RUN})
    assert resolve_run_selector(client, "run-1/build/verify/0") == ResolvedSelector(
        kind="verify", run_id="run-1", task_idx=0, verify_idx=0, verify_scope="task"
    )


def test_resolve_session_verify() -> None:
    client = _client_for({"/api/runs/run-1": _RUN})
    assert resolve_run_selector(client, "run-1/build/compile/verify/1") == ResolvedSelector(
        kind="verify",
        run_id="run-1",
        task_idx=0,
        session_idx=0,
        verify_idx=1,
        verify_scope="session",
    )


def test_resolve_verify_index_out_of_range_raises() -> None:
    client = _client_for({"/api/runs/run-1": _RUN})
    with pytest.raises(SelectorError, match="out of range"):
        resolve_run_selector(client, "run-1/build/verify/5")


# ── guardian/branch selectors ─────────────────────────────────────────────────


def test_parse_guardian_selector_bare_id() -> None:
    assert parse_guardian_selector("guardian-1") == ("guardian-1", None)


def test_parse_guardian_selector_with_branch() -> None:
    assert parse_guardian_selector("guardian-1#feature/a") == ("guardian-1", "feature/a")


def test_parse_guardian_selector_at_name() -> None:
    assert parse_guardian_selector("@my-review") == ("@my-review", None)


def test_resolve_bare_guardian_id_does_not_fetch() -> None:
    def handler(_request: httpx.Request) -> httpx.Response:
        raise AssertionError("should not fetch for a bare guardian id")

    client = DaemonClient("http://daemon.test", transport=httpx.MockTransport(handler))
    assert resolve_guardian_selector(client, "guardian-000000000001") == (
        ResolvedGuardianSelector("guardian-000000000001")
    )


def test_resolve_guardian_by_name() -> None:
    client = _client_for({"/api/guardians": _GUARDIANS})
    assert resolve_guardian_selector(client, "@my-review") == ResolvedGuardianSelector(
        "guardian-000000000001"
    )


def test_resolve_unknown_guardian_name_raises() -> None:
    client = _client_for({"/api/guardians": _GUARDIANS})
    with pytest.raises(SelectorError, match="no review named"):
        resolve_guardian_selector(client, "@nonexistent")


def test_resolve_branch_by_position() -> None:
    client = _client_for({"/api/guardians/guardian-000000000001": _GUARDIAN_DETAIL})
    assert resolve_guardian_selector(client, "guardian-000000000001#0") == (
        ResolvedGuardianSelector(
            "guardian-000000000001", branch_id="branch-000000000001", branch="feature/a"
        )
    )


def test_resolve_branch_by_name() -> None:
    client = _client_for({"/api/guardians/guardian-000000000001": _GUARDIAN_DETAIL})
    assert resolve_guardian_selector(client, "guardian-000000000001#feature/b") == (
        ResolvedGuardianSelector(
            "guardian-000000000001", branch_id="branch-000000000002", branch="feature/b"
        )
    )


def test_resolve_unknown_branch_raises() -> None:
    client = _client_for({"/api/guardians/guardian-000000000001": _GUARDIAN_DETAIL})
    with pytest.raises(SelectorError, match="no branch"):
        resolve_guardian_selector(client, "guardian-000000000001#feature/nonexistent")


def test_resolve_name_and_branch_together() -> None:
    client = _client_for(
        {
            "/api/guardians": _GUARDIANS,
            "/api/guardians/guardian-000000000001": _GUARDIAN_DETAIL,
        }
    )
    assert resolve_guardian_selector(client, "@my-review#feature/a") == (
        ResolvedGuardianSelector(
            "guardian-000000000001", branch_id="branch-000000000001", branch="feature/a"
        )
    )
