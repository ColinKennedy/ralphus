"""Tests for the daemon HTTP client, using an in-process mock transport."""

from __future__ import annotations

import httpx
import pytest

from ralphus.client import DaemonClient, DaemonError


def _client(handler: object) -> DaemonClient:
    assert callable(handler)
    transport = httpx.MockTransport(handler)
    return DaemonClient("http://daemon.test", transport=transport)


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
