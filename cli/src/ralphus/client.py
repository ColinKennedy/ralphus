"""HTTP client for the ralphus daemon API (see ``../../../docs/daemon-api.md``).

The CLI holds no state; it is a thin, typed wrapper over the daemon's endpoints.
"""

from __future__ import annotations

from dataclasses import dataclass
from typing import Any

import httpx

__all__ = ["DEFAULT_DAEMON_URL", "DaemonClient", "DaemonError", "ValidationOutcome"]

DEFAULT_DAEMON_URL = "http://127.0.0.1:7890"


class DaemonError(Exception):
    """Raised when the daemon returns an error or cannot be reached."""


@dataclass
class ValidationOutcome:
    """The result of a validate request."""

    valid: bool
    errors: list[dict[str, Any]]
    warnings: list[dict[str, Any]]


class DaemonClient:
    """A typed client for the daemon's HTTP/JSON API."""

    def __init__(
        self, base_url: str = DEFAULT_DAEMON_URL, *, transport: httpx.BaseTransport | None = None
    ) -> None:
        self._client = httpx.Client(base_url=base_url, timeout=10.0, transport=transport)

    def close(self) -> None:
        """Close the underlying HTTP connection."""
        self._client.close()

    def __enter__(self) -> DaemonClient:
        return self

    def __exit__(self, *_exc: object) -> None:
        self.close()

    def _get(self, path: str) -> Any:
        try:
            resp = self._client.get(path)
        except httpx.HTTPError as exc:
            raise DaemonError(f"could not reach daemon: {exc}") from exc
        return self._json_or_raise(resp)

    def _post(self, path: str, payload: dict[str, Any] | None = None) -> Any:
        try:
            resp = self._client.post(path, json=payload)
        except httpx.HTTPError as exc:
            raise DaemonError(f"could not reach daemon: {exc}") from exc
        return self._json_or_raise(resp)

    @staticmethod
    def _json_or_raise(resp: httpx.Response) -> Any:
        try:
            body = resp.json()
        except ValueError:
            body = None
        if resp.is_success:
            return body
        message = _extract_error_message(body) or f"HTTP {resp.status_code}"
        raise DaemonError(message)

    def health(self) -> dict[str, Any]:
        """Return the daemon health object."""
        result: dict[str, Any] = self._get("/api/daemon")
        return result

    def validate(self, toml_text: str) -> ValidationOutcome:
        """Validate a TOML batch without submitting it."""
        data = self._post("/api/runs/validate", {"toml": toml_text})
        return ValidationOutcome(
            valid=bool(data.get("valid")),
            errors=list(data.get("errors", [])),
            warnings=list(data.get("warnings", [])),
        )

    def submit(
        self, toml_text: str, *, hold: bool = False, label: str | None = None
    ) -> dict[str, Any]:
        """Submit a TOML batch, returning ``{run_id, state}``."""
        payload: dict[str, Any] = {"toml": toml_text, "hold": hold}
        if label is not None:
            payload["label"] = label
        result: dict[str, Any] = self._post("/api/runs", payload)
        return result

    def tasks(self) -> dict[str, Any]:
        """Return the full board state."""
        result: dict[str, Any] = self._get("/api/tasks")
        return result

    def run(self, run_id: str) -> dict[str, Any]:
        """Return a single run's detail."""
        result: dict[str, Any] = self._get(f"/api/runs/{run_id}")
        return result

    def cancel(self, run_id: str) -> dict[str, Any]:
        """Cancel a run."""
        result: dict[str, Any] = self._post(f"/api/runs/{run_id}/cancel")
        return result

    def clear(
        self, *, states: list[str] | None = None, keep_temporary: bool = False
    ) -> dict[str, Any]:
        """Bulk-clear tasks and reviews.

        With no ``states`` filter this wipes everything and resets id sequences;
        a non-empty ``states`` list deletes only runs in those states. Returns
        ``{runs_deleted, guardians_deleted, worktrees_purged}``.
        """
        payload: dict[str, Any] = {"keep_temporary": keep_temporary}
        if states:
            payload["states"] = states
        result: dict[str, Any] = self._post("/api/clear", payload)
        return result


def _extract_error_message(body: Any) -> str | None:
    if isinstance(body, dict):
        error = body.get("error")
        if isinstance(error, dict):
            message = error.get("message")
            if isinstance(message, str):
                return message
    return None
