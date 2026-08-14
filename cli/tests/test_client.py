"""Tests for the daemon HTTP client, using an in-process mock transport."""

from __future__ import annotations

import json

import httpx
import pytest

from ralphus.client import DEFAULT_DAEMON_TIMEOUT, DaemonClient, DaemonError


def _client(handler: object) -> DaemonClient:
    assert callable(handler)
    transport = httpx.MockTransport(handler)
    return DaemonClient("http://daemon.test", transport=transport)


def test_default_timeout_is_generous_not_a_health_check_timeout() -> None:
    with DaemonClient("http://daemon.test") as client:
        assert client._client.timeout.read == DEFAULT_DAEMON_TIMEOUT


def test_timeout_env_var_override(monkeypatch: pytest.MonkeyPatch) -> None:
    monkeypatch.setenv("RALPHUS_DAEMON_TIMEOUT", "5")
    with DaemonClient("http://daemon.test") as client:
        assert client._client.timeout.read == 5.0


def test_explicit_timeout_wins_over_env_var(monkeypatch: pytest.MonkeyPatch) -> None:
    monkeypatch.setenv("RALPHUS_DAEMON_TIMEOUT", "5")
    with DaemonClient("http://daemon.test", timeout=90.0) as client:
        assert client._client.timeout.read == 90.0


def test_health() -> None:
    def handler(request: httpx.Request) -> httpx.Response:
        assert request.url.path == "/api/daemon"
        return httpx.Response(200, json={"name": "ralphus-daemon", "status": "ok"})

    with _client(handler) as client:
        assert client.health()["status"] == "ok"


def test_validate_valid() -> None:
    def handler(_request: httpx.Request) -> httpx.Response:
        return httpx.Response(200, json={"valid": True, "errors": [], "warnings": []})

    with _client(handler) as client:
        outcome = client.validate("[[task]]")
        assert outcome.valid
        assert outcome.errors == []


def test_validate_invalid_carries_errors() -> None:
    def handler(_request: httpx.Request) -> httpx.Response:
        return httpx.Response(
            200,
            json={"valid": False, "errors": [{"line": 2, "message": "bad"}], "warnings": []},
        )

    with _client(handler) as client:
        outcome = client.validate("junk")
        assert not outcome.valid
        assert outcome.errors[0]["line"] == 2


def test_submit_returns_run_id() -> None:
    def handler(request: httpx.Request) -> httpx.Response:
        assert request.url.path == "/api/runs"
        return httpx.Response(201, json={"run_id": "run-000000000001", "state": "pending"})

    with _client(handler) as client:
        result = client.submit("[[task]]", label="x")
        assert result["run_id"] == "run-000000000001"


def test_error_envelope_becomes_daemon_error() -> None:
    def handler(_request: httpx.Request) -> httpx.Response:
        return httpx.Response(
            400, json={"error": {"code": "validation_failed", "message": "the TOML is invalid"}}
        )

    with _client(handler) as client, pytest.raises(DaemonError, match="the TOML is invalid"):
        client.submit("junk")


def test_unreachable_daemon_raises() -> None:
    def handler(_request: httpx.Request) -> httpx.Response:
        raise httpx.ConnectError("refused")

    with _client(handler) as client, pytest.raises(DaemonError, match="could not reach daemon"):
        client.health()


def test_daemon_error_carries_status_code() -> None:
    def handler(_request: httpx.Request) -> httpx.Response:
        return httpx.Response(404, json={"error": {"code": "not_found", "message": "no run"}})

    with _client(handler) as client:
        try:
            client.run("run-nope")
        except DaemonError as exc:
            assert exc.status_code == 404
        else:
            pytest.fail("expected DaemonError")


def test_unreachable_daemon_error_has_no_status_code() -> None:
    def handler(_request: httpx.Request) -> httpx.Response:
        raise httpx.ConnectError("refused")

    with _client(handler) as client:
        try:
            client.health()
        except DaemonError as exc:
            assert exc.status_code is None
        else:
            pytest.fail("expected DaemonError")


# ── runs.* (Phase 0c) ─────────────────────────────────────────────────────────


def test_run_worktrees() -> None:
    def handler(request: httpx.Request) -> httpx.Response:
        assert request.url.path == "/api/runs/run-1/worktrees"
        return httpx.Response(200, json=[{"task_idx": 0, "session_idx": 0, "worktree": None}])

    with _client(handler) as client:
        result = client.run_worktrees("run-1")
        assert result == [{"task_idx": 0, "session_idx": 0, "worktree": None}]


def test_run_logs() -> None:
    def handler(request: httpx.Request) -> httpx.Response:
        assert request.url.path == "/api/runs/run-1/logs"
        return httpx.Response(200, json=[{"message": "run submitted"}])

    with _client(handler) as client:
        assert client.run_logs("run-1") == [{"message": "run submitted"}]


def test_session_pane() -> None:
    def handler(request: httpx.Request) -> httpx.Response:
        assert request.url.path == "/api/runs/run-1/sessions/0/1/pane"
        assert request.url.params["lines"] == "20000"
        return httpx.Response(200, json={"active": True, "content": "hello\n"})

    with _client(handler) as client:
        result = client.session_pane("run-1", 0, 1)
        assert result == {"active": True, "content": "hello\n"}


def test_verify_pane() -> None:
    def handler(request: httpx.Request) -> httpx.Response:
        assert request.url.path == "/api/runs/run-1/verifies/0/session/1/2/pane"
        return httpx.Response(200, json={"active": False, "content": ""})

    with _client(handler) as client:
        result = client.verify_pane("run-1", 0, "session", 1, 2)
        assert result == {"active": False, "content": ""}


def test_ghost_get() -> None:
    def handler(request: httpx.Request) -> httpx.Response:
        assert request.url.path == "/api/ghosts/session:run-1:0:1"
        return httpx.Response(200, json={"owner_uri": "session:run-1:0:1", "content": "note"})

    with _client(handler) as client:
        result = client.ghost_get("session:run-1:0:1")
        assert result == {"owner_uri": "session:run-1:0:1", "content": "note"}


def test_ghost_get_missing_raises_404() -> None:
    def handler(request: httpx.Request) -> httpx.Response:
        return httpx.Response(404, json={"error": "no such ghost"})

    with _client(handler) as client:
        with pytest.raises(DaemonError) as exc_info:
            client.ghost_get("session:run-1:0:1")
        assert exc_info.value.status_code == 404


def test_activate_run() -> None:
    def handler(request: httpx.Request) -> httpx.Response:
        assert request.method == "POST"
        assert request.url.path == "/api/runs/run-1/activate"
        return httpx.Response(200, json={"state": "pending"})

    with _client(handler) as client:
        assert client.activate_run("run-1")["state"] == "pending"


def test_restart_session_verify() -> None:
    def handler(request: httpx.Request) -> httpx.Response:
        assert request.url.path == "/api/runs/run-1/sessions/0/1/verify/2/restart"
        return httpx.Response(200, json={"state": "pending", "dirtied": []})

    with _client(handler) as client:
        result = client.restart_session_verify("run-1", 0, 1, 2)
        assert result["state"] == "pending"


def test_restart_task() -> None:
    def handler(request: httpx.Request) -> httpx.Response:
        assert request.method == "POST"
        assert request.url.path == "/api/runs/run-1/tasks/0/restart"
        return httpx.Response(200, json={"state": "pending", "dirtied": []})

    with _client(handler) as client:
        result = client.restart_task("run-1", 0)
        assert result["state"] == "pending"


def test_set_run_env_sends_set_and_unset() -> None:
    def handler(request: httpx.Request) -> httpx.Response:
        assert request.method == "POST"
        assert request.url.path == "/api/runs/run-1/env"
        body = json.loads(request.content)
        assert body == {"set": {"A": "1"}, "unset": ["B"]}
        return httpx.Response(200, json={"A": "1"})

    with _client(handler) as client:
        result = client.set_run_env("run-1", set_vars={"A": "1"}, unset_vars=["B"])
        assert result == {"A": "1"}


def test_set_run_env_defaults_missing_args_to_empty() -> None:
    def handler(request: httpx.Request) -> httpx.Response:
        body = json.loads(request.content)
        assert body == {"set": {}, "unset": []}
        return httpx.Response(200, json={})

    with _client(handler) as client:
        client.set_run_env("run-1")


def test_set_task_env_sends_set_and_unset() -> None:
    def handler(request: httpx.Request) -> httpx.Response:
        assert request.method == "POST"
        assert request.url.path == "/api/runs/run-1/tasks/0/env"
        body = json.loads(request.content)
        assert body == {"set": {"A": "1"}, "unset": ["B"]}
        return httpx.Response(200, json={"A": "1"})

    with _client(handler) as client:
        result = client.set_task_env("run-1", 0, set_vars={"A": "1"}, unset_vars=["B"])
        assert result == {"A": "1"}


def test_set_task_verify_env_sends_set_and_unset() -> None:
    def handler(request: httpx.Request) -> httpx.Response:
        assert request.method == "POST"
        assert request.url.path == "/api/runs/run-1/tasks/0/verify/env"
        body = json.loads(request.content)
        assert body == {"set": {"A": "1"}, "unset": []}
        return httpx.Response(200, json={"A": "1"})

    with _client(handler) as client:
        result = client.set_task_verify_env("run-1", 0, set_vars={"A": "1"})
        assert result == {"A": "1"}


def test_set_session_env_sends_set_and_unset() -> None:
    def handler(request: httpx.Request) -> httpx.Response:
        assert request.method == "POST"
        assert request.url.path == "/api/runs/run-1/sessions/0/1/env"
        body = json.loads(request.content)
        assert body == {"set": {"A": "1"}, "unset": ["B"]}
        return httpx.Response(200, json={"A": "1"})

    with _client(handler) as client:
        result = client.set_session_env("run-1", 0, 1, set_vars={"A": "1"}, unset_vars=["B"])
        assert result == {"A": "1"}


def test_set_session_verify_env_sends_set_and_unset() -> None:
    def handler(request: httpx.Request) -> httpx.Response:
        assert request.method == "POST"
        assert request.url.path == "/api/runs/run-1/sessions/0/1/verify/env"
        body = json.loads(request.content)
        assert body == {"set": {}, "unset": ["A"]}
        return httpx.Response(200, json={})

    with _client(handler) as client:
        client.set_session_verify_env("run-1", 0, 1, unset_vars=["A"])


def test_edit_session_prefers_command_over_prompt() -> None:
    def handler(request: httpx.Request) -> httpx.Response:
        assert request.url.path == "/api/runs/run-1/edit"
        body = json.loads(request.content)
        assert body["kind"] == "session"
        assert body["task_idx"] == 0
        assert body["session_idx"] == 1
        assert body["command"] == "make test"
        assert "prompt" not in body
        return httpx.Response(200, json={"id": "run-1"})

    with _client(handler) as client:
        client.edit_session("run-1", 0, 1, command="make test")


def test_add_dependency() -> None:
    def handler(request: httpx.Request) -> httpx.Response:
        assert request.method == "POST"
        assert request.url.path == "/api/runs/run-2/add-dependency"
        body = json.loads(request.content)
        assert body == {"target_id": "run-1"}
        return httpx.Response(200, json={"id": "run-2", "state": "pending"})

    with _client(handler) as client:
        assert client.add_dependency("run-2", "run-1")["id"] == "run-2"


def test_add_dependency_cycle_raises_daemon_error() -> None:
    def handler(_request: httpx.Request) -> httpx.Response:
        return httpx.Response(
            409,
            json={
                "error": {
                    "code": "invalid_transition",
                    "message": "adding a dependency on run-1 would create a cycle",
                }
            },
        )

    with _client(handler) as client:
        try:
            client.add_dependency("run-1", "run-2")
        except DaemonError as exc:
            assert exc.status_code == 409
            assert "cycle" in str(exc)
        else:
            pytest.fail("expected DaemonError")


def test_delete_run() -> None:
    def handler(request: httpx.Request) -> httpx.Response:
        assert request.method == "DELETE"
        assert request.url.path == "/api/runs/run-1"
        return httpx.Response(200, json={"state": "deleted"})

    with _client(handler) as client:
        assert client.delete_run("run-1")["state"] == "deleted"


# ── guardians.* (Phase 0c) ────────────────────────────────────────────────────


def test_guardian_list_returns_a_bare_array() -> None:
    # GET /api/guardians serializes `Vec<GuardianView>` directly -- not wrapped
    # in `{"guardians": [...]}`.
    def handler(request: httpx.Request) -> httpx.Response:
        assert request.url.path == "/api/guardians"
        return httpx.Response(200, json=[{"id": "guardian-1", "name": "x"}])

    with _client(handler) as client:
        assert client.guardian_list() == [{"id": "guardian-1", "name": "x"}]


def test_guardian_logs_returns_a_bare_array() -> None:
    def handler(request: httpx.Request) -> httpx.Response:
        assert request.url.path == "/api/guardians/guardian-1/logs"
        return httpx.Response(200, json=[{"message": "created"}])

    with _client(handler) as client:
        assert client.guardian_logs("guardian-1") == [{"message": "created"}]


def test_guardian_create() -> None:
    def handler(request: httpx.Request) -> httpx.Response:
        assert request.url.path == "/api/guardians"
        body = json.loads(request.content)
        assert body == {
            "name": "my-review",
            "base_branch": "main",
            "git_root": "/repo",
            "checks": [],
            "skip_auto_build": False,
            "skip_worktree_checks": False,
            "skip_worktrees": False,
        }
        return httpx.Response(201, json={"id": "guardian-1"})

    with _client(handler) as client:
        assert client.guardian_create("my-review", "main", "/repo")["id"] == "guardian-1"


def test_guardian_reorder_with_enabled_map() -> None:
    def handler(request: httpx.Request) -> httpx.Response:
        assert request.url.path == "/api/guardians/guardian-1/branches/reorder"
        body = json.loads(request.content)
        assert body == {"order": ["b", "a"], "enabled": {"a": False}}
        return httpx.Response(200, json={"id": "guardian-1"})

    with _client(handler) as client:
        client.guardian_reorder("guardian-1", ["b", "a"], enabled={"a": False})


def test_guardian_chat_fork() -> None:
    def handler(request: httpx.Request) -> httpx.Response:
        assert request.url.path == "/api/guardians/guardian-1/chat/fork"
        body = json.loads(request.content)
        assert body == {"seq": 3, "text": "retry"}
        return httpx.Response(200, json={"ok": True})

    with _client(handler) as client:
        client.guardian_chat_fork("guardian-1", 3, "retry")


def test_guardian_settings_only_sends_present_fields() -> None:
    def handler(request: httpx.Request) -> httpx.Response:
        assert request.url.path == "/api/guardians/guardian-1/settings"
        body = json.loads(request.content)
        assert body == {"skip_auto_build": True}
        return httpx.Response(200, json={"id": "guardian-1"})

    with _client(handler) as client:
        client.guardian_settings("guardian-1", skip_auto_build=True)


def test_guardian_base_branches() -> None:
    def handler(request: httpx.Request) -> httpx.Response:
        assert request.url.path == "/api/guardians/guardian-1/base-branches"
        return httpx.Response(200, json=["origin/main", "origin/develop"])

    with _client(handler) as client:
        assert client.guardian_base_branches("guardian-1") == ["origin/main", "origin/develop"]


def test_guardian_submit_prs() -> None:
    def handler(request: httpx.Request) -> httpx.Response:
        assert request.url.path == "/api/guardians/guardian-1/pull-requests"
        body = json.loads(request.content)
        assert body == {"prs": [{"branch_id": "branch-000000000001"}]}
        return httpx.Response(202, json={"status": "submitting"})

    with _client(handler) as client:
        result = client.guardian_submit_prs("guardian-1", [{"branch_id": "branch-000000000001"}])
        assert result == {"status": "submitting"}


def test_guardian_list_prs_returns_a_bare_array() -> None:
    def handler(request: httpx.Request) -> httpx.Response:
        assert request.url.path == "/api/guardians/guardian-1/pull-requests"
        return httpx.Response(200, json=[{"id": "pr-1"}])

    with _client(handler) as client:
        assert client.guardian_list_prs("guardian-1") == [{"id": "pr-1"}]


def test_pr_get() -> None:
    def handler(request: httpx.Request) -> httpx.Response:
        assert request.url.path == "/api/pull-requests/pr-1"
        return httpx.Response(200, json={"id": "pr-1", "title": "Add foo"})

    with _client(handler) as client:
        assert client.pr_get("pr-1")["title"] == "Add foo"


def test_pr_find_encodes_query_params() -> None:
    def handler(request: httpx.Request) -> httpx.Response:
        assert request.url.path == "/api/pull-requests"
        assert dict(request.url.params) == {
            "forge": "gitlab",
            "repo": "group/proj",
            "pr_number": "7",
        }
        return httpx.Response(200, json={"id": "pr-1"})

    with _client(handler) as client:
        result = client.pr_find("gitlab", "group/proj", 7)
        assert result == {"id": "pr-1"}


def test_pr_update_only_sends_present_fields() -> None:
    def handler(request: httpx.Request) -> httpx.Response:
        assert request.url.path == "/api/pull-requests/pr-1"
        body = json.loads(request.content)
        assert body == {"pr_number": 43, "state": "closed"}
        return httpx.Response(200, json={"id": "pr-1", "pr_number": 43})

    with _client(handler) as client:
        client.pr_update("pr-1", pr_number=43, state="closed")


def test_pr_comments_returns_a_bare_array() -> None:
    def handler(request: httpx.Request) -> httpx.Response:
        assert request.url.path == "/api/pull-requests/pr-1/comments"
        return httpx.Response(200, json=[{"external_id": "c1", "actioned": False}])

    with _client(handler) as client:
        assert client.pr_comments("pr-1") == [{"external_id": "c1", "actioned": False}]


def test_pr_action_feedback() -> None:
    def handler(request: httpx.Request) -> httpx.Response:
        assert request.url.path == "/api/pull-requests/pr-1/action-feedback"
        return httpx.Response(202, json={"status": "actioning_feedback"})

    with _client(handler) as client:
        result = client.pr_action_feedback("pr-1")
        assert result == {"status": "actioning_feedback"}


def test_register_project_posts_to_api_projects() -> None:
    def handler(request: httpx.Request) -> httpx.Response:
        assert request.url.path == "/api/projects"
        assert json.loads(request.content) == {
            "name": "my-project",
            "path": "/repo",
            "description": "desc",
            "vcs": "git",
        }
        return httpx.Response(201, json={"name": "my-project"})

    with _client(handler) as client:
        result = client.register_project("my-project", "/repo", description="desc")
        assert result["name"] == "my-project"


def test_list_projects_returns_registered() -> None:
    def handler(request: httpx.Request) -> httpx.Response:
        assert request.url.path == "/api/projects"
        return httpx.Response(200, json={"projects": [{"name": "my-project"}]})

    with _client(handler) as client:
        result = client.list_projects()
        assert result["projects"][0]["name"] == "my-project"


def test_get_project_returns_details() -> None:
    def handler(request: httpx.Request) -> httpx.Response:
        assert request.url.path == "/api/projects/my-project"
        return httpx.Response(
            200,
            json={
                "name": "my-project",
                "description": "desc",
                "path": "/repo",
                "vcs": "git",
                "created_at_ms": 0,
            },
        )

    with _client(handler) as client:
        result = client.get_project("my-project")
        assert result["path"] == "/repo"
        assert result["vcs"] == "git"


def test_get_project_missing_raises_daemon_error() -> None:
    def handler(_request: httpx.Request) -> httpx.Response:
        message = 'project "nope" is not registered'
        return httpx.Response(404, json={"error": {"code": "not_found", "message": message}})

    with _client(handler) as client, pytest.raises(DaemonError, match="not registered"):
        client.get_project("nope")
