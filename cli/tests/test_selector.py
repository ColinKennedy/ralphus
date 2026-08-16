"""Tests for the run/task/session/verify and guardian/branch selector grammar."""

from __future__ import annotations

import httpx
import pytest

from ralphus.client import DaemonClient
from ralphus.selector import (
    RawGuardianSelector,
    RawRunSelector,
    ResolvedGuardianSelector,
    ResolvedSelector,
    SelectorError,
    guardian_view_uri,
    parse_guardian_selector,
    parse_run_selector,
    resolve_guardian_selector,
    resolve_run_selector,
    run_view_uri,
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
    assert parse_guardian_selector("guardian-1") == RawGuardianSelector("guardian-1")


def test_parse_guardian_selector_with_branch() -> None:
    assert parse_guardian_selector("guardian-1#feature/a") == RawGuardianSelector(
        "guardian-1", branch="feature/a"
    )


def test_parse_guardian_selector_accepts_either_branch_separator() -> None:
    """The legacy separator was `#`, RFC 3986's fragment delimiter. `~` -- the
    URI grammar's sigil -- is accepted here too; both still parse."""
    for raw in ("guardian-1~feature/a", "guardian-1#feature/a"):
        assert parse_guardian_selector(raw) == RawGuardianSelector("guardian-1", branch="feature/a")


def test_parse_guardian_selector_splits_at_the_earliest_separator() -> None:
    """A branch name containing the other separator still splits where the
    user meant it to, rather than at whichever token is 'preferred'."""
    assert parse_guardian_selector("@my review#fix~1") == RawGuardianSelector(
        "@my review", branch="fix~1"
    )


def test_parse_guardian_selector_at_name() -> None:
    assert parse_guardian_selector("@my-review") == RawGuardianSelector("@my-review")


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


# ── RAL-188 URI form: accepted by the same chokepoint as the legacy form ─────

_BOARD = {
    "runs": [
        {"id": "run-000000000001", "label": "nightly", "state": "running"},
        {"id": "run-000000000002", "label": "nightly", "state": "done"},
        {"id": "run-000000000003", "label": None, "state": "done"},
    ]
}

_URI_RUN = {
    "id": "run-000000000001",
    "label": "nightly",
    "state": "running",
    "tasks": [
        {
            "name": "build",
            "state": "done",
            "sessions": [{"id": "s0", "name": "compile", "verify": [{"id": "lint"}, {"id": None}]}],
            "verify": [{"id": "test"}],
        }
    ],
}


def test_uri_run_with_id_sidecar_does_not_fetch() -> None:
    def handler(_request: httpx.Request) -> httpx.Response:
        raise AssertionError("?id= is authoritative -- no lookup should happen")

    client = DaemonClient("http://daemon.test", transport=httpx.MockTransport(handler))
    assert resolve_run_selector(
        client, "ralphus:/RUN[nightly]?id=run-000000000001"
    ) == ResolvedSelector(kind="run", run_id="run-000000000001")


def test_uri_run_label_falls_back_to_the_id_when_unlabelled() -> None:
    client = _client_for({"/api/tasks": _BOARD})
    assert resolve_run_selector(client, "RUN[run-000000000003]") == ResolvedSelector(
        kind="run", run_id="run-000000000003"
    )


def test_uri_ambiguous_label_errors_and_lists_candidates() -> None:
    client = _client_for({"/api/tasks": _BOARD})
    with pytest.raises(SelectorError, match=r"matches 2 runs .*run-000000000002.*\?id="):
        resolve_run_selector(client, "RUN[nightly]")


def test_uri_unknown_label_raises() -> None:
    client = _client_for({"/api/tasks": _BOARD})
    with pytest.raises(SelectorError, match="no run labelled"):
        resolve_run_selector(client, "RUN[nope]")


def test_uri_resolves_task_session_and_named_verify() -> None:
    client = _client_for({"/api/runs/run-000000000001": _URI_RUN})
    uri = "ralphus:/RUN[nightly]/TASK[build]/SESSION[compile]/VERIFY[lint]?id=run-000000000001"
    assert resolve_run_selector(client, uri) == ResolvedSelector(
        kind="verify",
        run_id="run-000000000001",
        task_idx=0,
        session_idx=0,
        verify_idx=0,
        verify_scope="session",
    )


def test_uri_anonymous_verify_uses_the_positional_sigil() -> None:
    client = _client_for({"/api/runs/run-000000000001": _URI_RUN})
    uri = "ralphus:/RUN[nightly]/TASK[build]/SESSION[compile]/VERIFY[~1]?id=run-000000000001"
    assert resolve_run_selector(client, uri).verify_idx == 1


def test_uri_task_level_verify_has_no_session_segment() -> None:
    client = _client_for({"/api/runs/run-000000000001": _URI_RUN})
    uri = "ralphus:/RUN[nightly]/TASK[build]/VERIFY[test]?id=run-000000000001"
    assert resolve_run_selector(client, uri) == ResolvedSelector(
        kind="verify", run_id="run-000000000001", task_idx=0, verify_idx=0, verify_scope="task"
    )


def test_uri_bare_integer_is_a_name_not_an_index() -> None:
    """§C.4: unlike the legacy grammar, `TASK[0]` is the task *named* `0`."""
    client = _client_for({"/api/runs/run-000000000001": _URI_RUN})
    with pytest.raises(SelectorError, match="no task named '0'"):
        resolve_run_selector(client, "ralphus:/RUN[n]/TASK[0]?id=run-000000000001")


def test_uri_positional_task_index_still_works_with_the_sigil() -> None:
    client = _client_for({"/api/runs/run-000000000001": _URI_RUN})
    assert resolve_run_selector(
        client, "ralphus:/RUN[n]/TASK[~0]?id=run-000000000001"
    ) == ResolvedSelector(kind="task", run_id="run-000000000001", task_idx=0)


def test_uri_review_with_slash_in_the_label_parses() -> None:
    """§C.5: balanced `[...]` groups are extracted before the path is split."""
    parsed = parse_guardian_selector("ralphus:/REVIEW[RAL-174/175 batch]?id=guardian-000000000001")
    assert parsed == RawGuardianSelector(
        head="RAL-174/175 batch", guardian_id="guardian-000000000001", uri_form=True
    )


@pytest.mark.parametrize(
    "worktree",
    [
        pytest.param("~1", id="position"),
        pytest.param("feature/b", id="label"),
        pytest.param("branch-000000000002", id="branch-id"),
    ],
)
def test_uri_review_worktree_by_position_label_or_id(worktree: str) -> None:
    """`?worktree=` reaches one stacked branch three ways: its label (the
    feature branch name the Reviews UI lists it under), its stable `branch-...`
    id (§C.2), or `~<position>`."""
    client = _client_for({"/api/guardians/guardian-000000000001": _GUARDIAN_DETAIL})
    uri = f"ralphus:/REVIEW[my-review]?id=guardian-000000000001&worktree={worktree}"
    assert resolve_guardian_selector(client, uri) == ResolvedGuardianSelector(
        "guardian-000000000001", branch_id="branch-000000000002", branch="feature/b"
    )


def test_uri_review_worktree_label_containing_a_slash_survives_the_query_split() -> None:
    """`feature/a` is percent-encoded on the way out and decoded back on the
    way in, so a `/` in a branch label never re-splits the path."""
    client = _client_for({"/api/guardians/guardian-000000000001": _GUARDIAN_DETAIL})
    resolved = ResolvedGuardianSelector(
        "guardian-000000000001", branch_id="branch-000000000001", branch="feature/a"
    )
    uri = guardian_view_uri(_GUARDIAN_DETAIL, resolved)
    assert uri == "ralphus:/REVIEW[my-review]?id=guardian-000000000001&worktree=feature%2Fa"
    assert resolve_guardian_selector(client, uri) == resolved


def test_uri_review_combined_worktree() -> None:
    def handler(_request: httpx.Request) -> httpx.Response:
        raise AssertionError("?id= is authoritative -- no lookup should happen")

    client = DaemonClient("http://daemon.test", transport=httpx.MockTransport(handler))
    uri = "ralphus:/REVIEW[my-review]?id=guardian-000000000001&combined"
    assert resolve_guardian_selector(client, uri) == ResolvedGuardianSelector(
        "guardian-000000000001", combined=True
    )


def test_uri_run_family_is_not_mistaken_for_a_review() -> None:
    with pytest.raises(SelectorError, match="not a review"):
        parse_guardian_selector("ralphus:/RUN[nightly]?id=run-000000000001")


# ── URI production (§C.3: ralphus always emits the ?id= sidecar) ─────────────


def test_run_view_uri_round_trips_through_the_parser() -> None:
    client = _client_for({"/api/runs/run-000000000001": _URI_RUN})
    resolved = ResolvedSelector(
        kind="verify",
        run_id="run-000000000001",
        task_idx=0,
        session_idx=0,
        verify_idx=0,
        verify_scope="session",
    )
    uri = run_view_uri(_URI_RUN, resolved)
    assert uri == (
        "ralphus:/RUN[nightly]/TASK[build]/SESSION[compile]/VERIFY[lint]?id=run-000000000001"
    )
    assert resolve_run_selector(client, uri) == resolved


def test_run_view_uri_falls_back_to_the_index_for_an_anonymous_verify() -> None:
    resolved = ResolvedSelector(
        kind="verify",
        run_id="run-000000000001",
        task_idx=0,
        session_idx=0,
        verify_idx=1,
        verify_scope="session",
    )
    assert run_view_uri(_URI_RUN, resolved).endswith("/VERIFY[~1]?id=run-000000000001")


def test_run_view_uri_percent_encodes_a_label_containing_a_delimiter() -> None:
    run = {"id": "run-1", "label": "a/b[c]", "tasks": []}
    uri = run_view_uri(run, ResolvedSelector(kind="run", run_id="run-1"))
    assert uri == "ralphus:/RUN[a%2Fb%5Bc%5D]?id=run-1"
    assert parse_run_selector(uri).run_label == "a/b[c]"


def test_guardian_view_uri_round_trips_a_slash_bearing_name() -> None:
    guardian = {"id": "guardian-000000000001", "name": "RAL-174/175 batch"}
    uri = guardian_view_uri(guardian)
    assert uri == "ralphus:/REVIEW[RAL-174%2F175 batch]?id=guardian-000000000001"
    assert parse_guardian_selector(uri).head == "RAL-174/175 batch"
