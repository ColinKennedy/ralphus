"""HTTP client for the ralphus daemon API (see ``../../../docs/daemon-api.md``).

The CLI holds no state; it is a thin, typed wrapper over the daemon's endpoints.
"""

from __future__ import annotations

import os
from dataclasses import dataclass
from typing import Any
from urllib.parse import urlencode

import httpx

__all__ = [
    "DEFAULT_DAEMON_TIMEOUT",
    "DEFAULT_DAEMON_URL",
    "DaemonClient",
    "DaemonError",
    "ValidationOutcome",
]

DEFAULT_DAEMON_URL = "http://127.0.0.1:7890"
# A mutating request (e.g. submit) can involve TOML validation, project-registry
# lookups, and review-derivation git calls, all serialized behind the daemon's
# single-threaded HTTP loop -- a short timeout here reads as "daemon unreachable"
# even when the request is still being processed and will succeed. Overridable
# via $RALPHUS_DAEMON_TIMEOUT, matching $RALPHUS_DAEMON_URL.
DEFAULT_DAEMON_TIMEOUT = 60.0


class DaemonError(Exception):
    """Raised when the daemon returns an error or cannot be reached.

    ``status_code`` is the HTTP status the daemon returned (``None`` if the
    daemon could not be reached at all, e.g. connection refused). The CLI's
    exit-code mapping (``ralphus.output.exit_code_for``) keys off this field.
    """

    def __init__(self, message: str, *, status_code: int | None = None) -> None:
        super().__init__(message)
        self.status_code = status_code


@dataclass
class ValidationOutcome:
    """The result of a validate request."""

    valid: bool
    errors: list[dict[str, Any]]
    warnings: list[dict[str, Any]]


class DaemonClient:
    """A typed client for the daemon's HTTP/JSON API."""

    def __init__(
        self,
        base_url: str = DEFAULT_DAEMON_URL,
        *,
        timeout: float | None = None,
        transport: httpx.BaseTransport | None = None,
    ) -> None:
        if timeout is None:
            timeout = float(os.environ.get("RALPHUS_DAEMON_TIMEOUT", DEFAULT_DAEMON_TIMEOUT))
        self._client = httpx.Client(base_url=base_url, timeout=timeout, transport=transport)

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
        raise DaemonError(message, status_code=resp.status_code)

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

    def tasks(
        self, *, status: str | None = None, name: str | None = None, sort: str | None = None
    ) -> dict[str, Any]:
        """Return the board state, optionally filtered/sorted server-side.

        ``status`` is a comma-separated list of run states; ``name`` is a
        substring match on the run label; ``sort`` is ``"name"`` or omitted
        (newest-first, the default).
        """
        params: dict[str, str] = {}
        if status:
            params["status"] = status
        if name:
            params["name"] = name
        if sort:
            params["sort"] = sort
        path = f"/api/tasks?{urlencode(params)}" if params else "/api/tasks"
        result: dict[str, Any] = self._get(path)
        return result

    def run(self, run_id: str) -> dict[str, Any]:
        """Return a single run's detail."""
        result: dict[str, Any] = self._get(f"/api/runs/{run_id}")
        return result

    def cancel(self, run_id: str) -> dict[str, Any]:
        """Cancel a run."""
        result: dict[str, Any] = self._post(f"/api/runs/{run_id}/cancel")
        return result

    def queue(self) -> dict[str, Any]:
        """Return the classified, ordered queue of runnable work items."""
        result: dict[str, Any] = self._get("/api/queue")
        return result

    def queue_reorder(self, order: list[str]) -> dict[str, Any]:
        """Reorder the queue to match ``order`` (a list of item paths).

        The daemon stabilizes the order against dependencies before persisting
        it, so a dependency listed after its dependent is pulled back above it.
        Returns ``{order, items}`` with the repaired order and refreshed queue.
        """
        result: dict[str, Any] = self._post("/api/queue/reorder", {"order": order})
        return result

    def queue_set_position(
        self, items: list[str], position: int, *, absolute: bool = True
    ) -> dict[str, Any]:
        """Move ``items`` to ``position`` (absolute index or relative shift)."""
        payload: dict[str, Any] = {
            "items": items,
            "position": position,
            "absolute": absolute,
        }
        result: dict[str, Any] = self._post("/api/queue/set-position", payload)
        return result

    def set_status(
        self,
        run_id: str,
        state: str,
        *,
        kind: str = "run",
        task_idx: int = 0,
        session_idx: int = -1,
        verify_idx: int = -1,
        verify_scope: str = "",
    ) -> dict[str, Any]:
        """Manually override a run/task/session/verify node's status."""
        payload: dict[str, Any] = {
            "kind": kind,
            "task_idx": task_idx,
            "session_idx": session_idx,
            "verify_idx": verify_idx,
            "verify_scope": verify_scope,
            "state": state,
        }
        result: dict[str, Any] = self._post(f"/api/runs/{run_id}/set-status", payload)
        return result

    def register_project(
        self, name: str, path: str, *, description: str = "", vcs: str = "git"
    ) -> dict[str, Any]:
        """Register a project's on-disk location with the daemon (RAL-100).

        The daemon rejects a ``path`` that isn't a directory or isn't a git
        working tree. Returns the registered project record.
        """
        payload: dict[str, Any] = {
            "name": name,
            "path": path,
            "description": description,
            "vcs": vcs,
        }
        result: dict[str, Any] = self._post("/api/projects", payload)
        return result

    def list_projects(self) -> dict[str, Any]:
        """Return every registered project."""
        result: dict[str, Any] = self._get("/api/projects")
        return result

    def get_project(self, name: str) -> dict[str, Any]:
        """Return one registered project's details by its exact name.

        Raises :class:`DaemonError` (404) if no project is registered under
        that exact name.
        """
        result: dict[str, Any] = self._get(f"/api/projects/{name}")
        return result

    def register_machine(
        self,
        scheme: str,
        program: str,
        *,
        description: str = "",
        args: list[str] | None = None,
        protocol_version: int | None = None,
        supports_channel: bool = False,
    ) -> dict[str, Any]:
        """Register a machine provider with the daemon (RAL-185).

        ``scheme`` is the left half of a ``machine = "<scheme>:<uri>"`` value;
        ``program`` is the executable the daemon runs to reach that machine.
        Re-registering an existing scheme updates it in place.

        Registering is deliberately an administrative action reachable only
        here and over the API -- never from inside a submitted task file, since
        a TOML that could both name and define an executable would make
        ``submit`` equivalent to arbitrary code execution.
        """
        payload: dict[str, Any] = {
            "scheme": scheme,
            "program": program,
            "description": description,
            "args": args or [],
            "supports_channel": supports_channel,
        }
        if protocol_version is not None:
            payload["protocol_version"] = protocol_version
        result: dict[str, Any] = self._post("/api/machines", payload)
        return result

    def list_machines(self) -> dict[str, Any]:
        """Return every registered machine provider, plus the built-in schemes."""
        result: dict[str, Any] = self._get("/api/machines")
        return result

    def get_machine(self, scheme: str) -> dict[str, Any]:
        """Return one registered machine provider by its exact scheme.

        Raises :class:`DaemonError` (404) if no provider is registered under
        that scheme.
        """
        result: dict[str, Any] = self._get(f"/api/machines/{scheme}")
        return result

    def deregister_machine(self, scheme: str) -> dict[str, Any]:
        """Remove a registered machine provider.

        Raises :class:`DaemonError` (404) if it was not registered.
        """
        result: dict[str, Any] = self._delete(f"/api/machines/{scheme}")
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

    def _delete(self, path: str) -> Any:
        try:
            resp = self._client.delete(path)
        except httpx.HTTPError as exc:
            raise DaemonError(f"could not reach daemon: {exc}") from exc
        return self._json_or_raise(resp)

    # ── runs.* ────────────────────────────────────────────────────────────

    def run_graph(self, run_id: str) -> dict[str, Any]:
        """Return a run's internal session dependency graph as `{nodes, edges}`."""
        result: dict[str, Any] = self._get(f"/api/runs/{run_id}/graph")
        return result

    def global_graph(self, *, include_terminal: bool = False) -> dict[str, Any]:
        """Return the cross-run `[[default]]` gating graph as `{nodes, edges}`.

        ``include_terminal`` also includes done/failed/cancelled runs;
        otherwise only active (queued/pending/running) runs are included.
        """
        path = "/api/graph?all=1" if include_terminal else "/api/graph"
        result: dict[str, Any] = self._get(path)
        return result

    def resources(self) -> dict[str, Any]:
        """Return per-task resource usage (CPU/RAM/GPU)."""
        result: dict[str, Any] = self._get("/api/resources")
        return result

    def run_worktrees(self, run_id: str) -> list[dict[str, Any]]:
        """Return the worktrees a run's sessions are using (one entry per session)."""
        result: list[dict[str, Any]] = self._get(f"/api/runs/{run_id}/worktrees")
        return result

    def run_logs(self, run_id: str) -> list[dict[str, Any]]:
        """Return the state-transition audit log for a run (latest 500, oldest-first)."""
        result: list[dict[str, Any]] = self._get(f"/api/runs/{run_id}/logs")
        return result

    def activate_run(self, run_id: str) -> dict[str, Any]:
        """Promote a held (queued) run to pending so it can be scheduled."""
        result: dict[str, Any] = self._post(f"/api/runs/{run_id}/activate")
        return result

    def retry_run(self, run_id: str) -> dict[str, Any]:
        """Re-run a run with its existing parameters (reset to pending)."""
        result: dict[str, Any] = self._post(f"/api/runs/{run_id}/retry")
        return result

    def restart_run(self, run_id: str) -> dict[str, Any]:
        """Restart a whole run; returns ``{state, dirtied}``."""
        result: dict[str, Any] = self._post(f"/api/runs/{run_id}/restart")
        return result

    def add_dependency(self, run_id: str, target_id: str) -> dict[str, Any]:
        """Add a cross-run dependency: ``run_id`` will not be scheduled until
        ``target_id`` completes (RAL-105).

        Appends to the same ``[[default]] depends_on`` list that ``ralphus
        submit`` populates from TOML, reusing the existing whole-run gating —
        no new scheduling path. Raises ``DaemonError`` (status 409) for a
        self-reference or a reference that would create a dependency cycle,
        and (status 404) if either run does not exist.
        """
        result: dict[str, Any] = self._post(
            f"/api/runs/{run_id}/add-dependency", {"target_id": target_id}
        )
        return result

    def session_pane(
        self, run_id: str, task_idx: int, session_idx: int, *, lines: int = 20000
    ) -> dict[str, Any]:
        """Live tmux pane content for a task session: ``{active, content}``.

        ``active`` is ``false`` once the tmux session has ended (or never
        started) -- callers should treat that as "nothing to tail" rather
        than an error (see `ralphus history --live`).
        """
        result: dict[str, Any] = self._get(
            f"/api/runs/{run_id}/sessions/{task_idx}/{session_idx}/pane?lines={lines}"
        )
        return result

    def verify_pane(
        self,
        run_id: str,
        task_idx: int,
        scope: str,
        session_idx: int,
        verify_idx: int,
        *,
        lines: int = 20000,
    ) -> dict[str, Any]:
        """Live tmux pane content for a `prompt`-kind verify step: ``{active, content}``."""
        result: dict[str, Any] = self._get(
            f"/api/runs/{run_id}/verifies/{task_idx}/{scope}/{session_idx}/{verify_idx}/pane"
            f"?lines={lines}"
        )
        return result

    def ghost_get(self, owner_uri: str) -> dict[str, Any]:
        """The RAL-136 ghost (handoff note) published for ``owner_uri`` (e.g.
        ``session:<run_id>:<task_idx>:<session_idx>``): a ``GhostView`` dict
        (``content``, ``revision``, ...). Raises ``DaemonError`` with
        ``status_code == 404`` if nothing has been published for it yet --
        `ralphus history` (RAL-140) treats that as "no history recorded yet"
        rather than an error.
        """
        result: dict[str, Any] = self._get(f"/api/ghosts/{owner_uri}")
        return result

    def restart_session(self, run_id: str, task_idx: int, session_idx: int) -> dict[str, Any]:
        """Restart a single session (and its downstream); returns ``{state, dirtied}``."""
        result: dict[str, Any] = self._post(
            f"/api/runs/{run_id}/sessions/{task_idx}/{session_idx}/restart"
        )
        return result

    def restart_session_verify(
        self, run_id: str, task_idx: int, session_idx: int, verify_idx: int
    ) -> dict[str, Any]:
        """Restart a session's verify steps from ``verify_idx`` onwards."""
        result: dict[str, Any] = self._post(
            f"/api/runs/{run_id}/sessions/{task_idx}/{session_idx}/verify/{verify_idx}/restart"
        )
        return result

    def restart_task_verify(self, run_id: str, task_idx: int, verify_idx: int) -> dict[str, Any]:
        """Restart a task's verify steps from ``verify_idx`` onwards."""
        result: dict[str, Any] = self._post(
            f"/api/runs/{run_id}/tasks/{task_idx}/verify/{verify_idx}/restart"
        )
        return result

    def restart_task(self, run_id: str, task_idx: int) -> dict[str, Any]:
        """Restart a whole task (its sessions plus their downstream, RAL-150);
        returns ``{state, dirtied}``.
        """
        result: dict[str, Any] = self._post(f"/api/runs/{run_id}/tasks/{task_idx}/restart")
        return result

    def set_run_env(
        self,
        run_id: str,
        *,
        set_vars: dict[str, str] | None = None,
        unset_vars: list[str] | None = None,
    ) -> dict[str, str]:
        """Add/replace (``set_vars``) and remove (``unset_vars``) a run's
        persistent environment-variable overrides (RAL-150). Returns the
        resulting map.

        Overrides persist across any number of retries until explicitly
        unset -- they take effect the next time the daemon executes one of
        the run's sessions or verify steps, not immediately. Pair this with
        ``retry_run``/``restart_run``/``restart_task``/``restart_session`` (or
        the CLI's ``ralphus retry <selector> --environment ...``, which does
        exactly that) to actually re-run something under the new values.
        """
        payload: dict[str, Any] = {"set": set_vars or {}, "unset": unset_vars or []}
        result: dict[str, str] = self._post(f"/api/runs/{run_id}/env", payload)
        return result

    def set_task_env(
        self,
        run_id: str,
        task_idx: int,
        *,
        set_vars: dict[str, str] | None = None,
        unset_vars: list[str] | None = None,
    ) -> dict[str, str]:
        """Add/replace (``set_vars``) and remove (``unset_vars``) a task's own
        persistent environment-variable overrides (hierarchical env
        overrides, extending RAL-150) -- merged on top of the owning run's,
        and merged onto every session under this task. Returns the resulting
        map.
        """
        payload: dict[str, Any] = {"set": set_vars or {}, "unset": unset_vars or []}
        result: dict[str, str] = self._post(f"/api/runs/{run_id}/tasks/{task_idx}/env", payload)
        return result

    def set_task_verify_env(
        self,
        run_id: str,
        task_idx: int,
        *,
        set_vars: dict[str, str] | None = None,
        unset_vars: list[str] | None = None,
    ) -> dict[str, str]:
        """Add/replace/remove the environment-variable overrides applied only
        to a task's own (task-scoped) verify steps -- merged on top of the
        task's own overrides (and the run's). Returns the resulting map.
        """
        payload: dict[str, Any] = {"set": set_vars or {}, "unset": unset_vars or []}
        result: dict[str, str] = self._post(
            f"/api/runs/{run_id}/tasks/{task_idx}/verify/env", payload
        )
        return result

    def set_session_env(
        self,
        run_id: str,
        task_idx: int,
        session_idx: int,
        *,
        set_vars: dict[str, str] | None = None,
        unset_vars: list[str] | None = None,
    ) -> dict[str, str]:
        """Add/replace/remove a session's own persistent environment-variable
        overrides -- merged on top of its task's/run's. Returns the resulting
        map.
        """
        payload: dict[str, Any] = {"set": set_vars or {}, "unset": unset_vars or []}
        result: dict[str, str] = self._post(
            f"/api/runs/{run_id}/sessions/{task_idx}/{session_idx}/env", payload
        )
        return result

    def set_session_verify_env(
        self,
        run_id: str,
        task_idx: int,
        session_idx: int,
        *,
        set_vars: dict[str, str] | None = None,
        unset_vars: list[str] | None = None,
    ) -> dict[str, str]:
        """Add/replace/remove the environment-variable overrides applied only
        to a session's own (session-scoped) verify steps -- merged on top of
        the session's own overrides (and its task's/run's). Returns the
        resulting map.
        """
        payload: dict[str, Any] = {"set": set_vars or {}, "unset": unset_vars or []}
        result: dict[str, str] = self._post(
            f"/api/runs/{run_id}/sessions/{task_idx}/{session_idx}/verify/env", payload
        )
        return result

    def set_task_verify_step_env(
        self,
        run_id: str,
        task_idx: int,
        verify_idx: int,
        *,
        set_vars: dict[str, str] | None = None,
        unset_vars: list[str] | None = None,
    ) -> dict[str, str]:
        """Add/replace/remove the environment-variable overrides applied to
        **one individual** task-scoped verify step (RAL-191) -- the narrowest
        layer, merged on top of the task-verify scope's (and its ancestors').
        Returns the resulting map.
        """
        payload: dict[str, Any] = {"set": set_vars or {}, "unset": unset_vars or []}
        result: dict[str, str] = self._post(
            f"/api/runs/{run_id}/tasks/{task_idx}/verify/{verify_idx}/env", payload
        )
        return result

    def set_session_verify_step_env(
        self,
        run_id: str,
        task_idx: int,
        session_idx: int,
        verify_idx: int,
        *,
        set_vars: dict[str, str] | None = None,
        unset_vars: list[str] | None = None,
    ) -> dict[str, str]:
        """Add/replace/remove the environment-variable overrides applied to
        **one individual** session-scoped verify step (RAL-191) -- the
        narrowest layer, merged on top of the session-verify scope's (and its
        ancestors'). Returns the resulting map.
        """
        payload: dict[str, Any] = {"set": set_vars or {}, "unset": unset_vars or []}
        result: dict[str, str] = self._post(
            f"/api/runs/{run_id}/sessions/{task_idx}/{session_idx}/verify/{verify_idx}/env",
            payload,
        )
        return result

    def edit_run(self, run_id: str, *, label: str | None) -> dict[str, Any]:
        """Rename a run's label; resets the run to pending."""
        payload: dict[str, Any] = {"kind": "run", "label": label}
        result: dict[str, Any] = self._post(f"/api/runs/{run_id}/edit", payload)
        return result

    def edit_task(
        self, run_id: str, task_idx: int, *, name: str | None, project: str | None
    ) -> dict[str, Any]:
        """Edit a task node's name/project; resets the run to pending."""
        payload: dict[str, Any] = {"kind": "task", "task_idx": task_idx}
        if name is not None:
            payload["name"] = name
        if project is not None:
            payload["project"] = project
        result: dict[str, Any] = self._post(f"/api/runs/{run_id}/edit", payload)
        return result

    def edit_session(
        self,
        run_id: str,
        task_idx: int,
        session_idx: int,
        *,
        cwd: str | None = None,
        agent: str | None = None,
        model: str | None = None,
        prompt: str | None = None,
        command: str | None = None,
    ) -> dict[str, Any]:
        """Edit a session's fields (prompt XOR command); resets this session and
        its downstream dependents to pending (upstream/sibling sessions that
        already finished are left alone)."""
        payload: dict[str, Any] = {
            "kind": "session",
            "task_idx": task_idx,
            "session_idx": session_idx,
        }
        for key, value in (
            ("cwd", cwd),
            ("agent", agent),
            ("model", model),
            ("prompt", prompt),
            ("command", command),
        ):
            if value is not None:
                payload[key] = value
        result: dict[str, Any] = self._post(f"/api/runs/{run_id}/edit", payload)
        return result

    def delete_run(self, run_id: str) -> dict[str, Any]:
        """Permanently delete a run."""
        result: dict[str, Any] = self._delete(f"/api/runs/{run_id}")
        return result

    # ── guardians.* ───────────────────────────────────────────────────────

    def guardian_list(self) -> list[dict[str, Any]]:
        """Return all reviews (guardians)."""
        result: list[dict[str, Any]] = self._get("/api/guardians")
        return result

    def guardian_get(self, guardian_id: str) -> dict[str, Any]:
        """Return a single review's detail."""
        result: dict[str, Any] = self._get(f"/api/guardians/{guardian_id}")
        return result

    def guardian_logs(self, guardian_id: str) -> list[dict[str, Any]]:
        """Return the state-transition audit log for a review (latest 500, oldest-first)."""
        result: list[dict[str, Any]] = self._get(f"/api/guardians/{guardian_id}/logs")
        return result

    def guardian_create(
        self,
        name: str,
        base_branch: str,
        git_root: str,
        *,
        checks: list[str] | None = None,
        skip_auto_build: bool = False,
        skip_worktree_checks: bool = False,
        skip_worktrees: bool = False,
        review_type: str | None = None,
    ) -> dict[str, Any]:
        """Create a new review; returns ``{id}``."""
        payload: dict[str, Any] = {
            "name": name,
            "base_branch": base_branch,
            "git_root": git_root,
            "checks": checks or [],
            "skip_auto_build": skip_auto_build,
            "skip_worktree_checks": skip_worktree_checks,
            "skip_worktrees": skip_worktrees,
        }
        if review_type is not None:
            payload["review_type"] = review_type
        result: dict[str, Any] = self._post("/api/guardians", payload)
        return result

    def guardian_rename(self, guardian_id: str, name: str) -> dict[str, Any]:
        """Rename a review."""
        result: dict[str, Any] = self._post(f"/api/guardians/{guardian_id}/rename", {"name": name})
        return result

    def guardian_settings(
        self,
        guardian_id: str,
        *,
        skip_auto_build: bool | None = None,
        skip_worktree_checks: bool | None = None,
        skip_worktrees: bool | None = None,
        resolver_agent: str | None = None,
        resolver_model: str | None = None,
        base_branch: str | None = None,
        auto_pr_feedback: bool | None = None,
    ) -> dict[str, Any]:
        """Update per-review opt-out settings; only present fields are changed."""
        payload: dict[str, Any] = {}
        for key, value in (
            ("skip_auto_build", skip_auto_build),
            ("skip_worktree_checks", skip_worktree_checks),
            ("skip_worktrees", skip_worktrees),
            ("resolver_agent", resolver_agent),
            ("resolver_model", resolver_model),
            ("base_branch", base_branch),
            ("auto_pr_feedback", auto_pr_feedback),
        ):
            if value is not None:
                payload[key] = value
        result: dict[str, Any] = self._post(f"/api/guardians/{guardian_id}/settings", payload)
        return result

    def guardian_squash(self, guardian_id: str, project: str, enabled: bool) -> dict[str, Any]:
        """Enable/disable per-commit squashing for one project within a review."""
        payload = {"project": project, "enabled": enabled}
        result: dict[str, Any] = self._post(f"/api/guardians/{guardian_id}/squash", payload)
        return result

    def guardian_delete(self, guardian_id: str) -> dict[str, Any]:
        """Delete a review and purge its worktrees/branches."""
        result: dict[str, Any] = self._delete(f"/api/guardians/{guardian_id}")
        return result

    def guardian_add_branch(self, guardian_id: str, branch: str) -> dict[str, Any]:
        """Add a branch to a review; returns ``{position}``."""
        result: dict[str, Any] = self._post(
            f"/api/guardians/{guardian_id}/branches", {"branch": branch}
        )
        return result

    def guardian_reorder(
        self, guardian_id: str, order: list[str], *, enabled: dict[str, bool] | None = None
    ) -> dict[str, Any]:
        """Reorder a review's branches, optionally toggling enabled state atomically."""
        payload: dict[str, Any] = {"order": order, "enabled": enabled or {}}
        result: dict[str, Any] = self._post(
            f"/api/guardians/{guardian_id}/branches/reorder", payload
        )
        return result

    def guardian_arrange(
        self, guardian_id: str, order: list[str], *, enabled: dict[str, bool] | None = None
    ) -> dict[str, Any]:
        """Atomically reorder/enable branches and kick off the rebase (C3).

        Equivalent to `guardian_reorder` followed by `guardian_merge`, but as
        one request -- no read-modify-write window between the two calls.
        """
        payload: dict[str, Any] = {"order": order, "enabled": enabled or {}}
        result: dict[str, Any] = self._post(
            f"/api/guardians/{guardian_id}/branches/arrange", payload
        )
        return result

    def guardian_feedback(self, guardian_id: str, branch_id: str, feedback: str) -> dict[str, Any]:
        """Post feedback on one branch, triggering a resolver re-attempt."""
        result: dict[str, Any] = self._post(
            f"/api/guardians/{guardian_id}/branches/{branch_id}/feedback",
            {"feedback": feedback},
        )
        return result

    def guardian_messages(self, guardian_id: str) -> dict[str, Any]:
        """Return the review's global feedback thread."""
        result: dict[str, Any] = self._get(f"/api/guardians/{guardian_id}/messages")
        return result

    def guardian_chat(
        self, guardian_id: str, text: str, *, image: str | None = None
    ) -> dict[str, Any]:
        """Post a reviewer message to the global feedback thread."""
        payload: dict[str, Any] = {"text": text}
        if image is not None:
            payload["image"] = image
        result: dict[str, Any] = self._post(f"/api/guardians/{guardian_id}/chat", payload)
        return result

    def guardian_chat_fork(
        self, guardian_id: str, seq: int, text: str, *, image: str | None = None
    ) -> dict[str, Any]:
        """Fork the feedback thread at ``seq``, replacing it with a new message."""
        payload: dict[str, Any] = {"seq": seq, "text": text}
        if image is not None:
            payload["image"] = image
        result: dict[str, Any] = self._post(f"/api/guardians/{guardian_id}/chat/fork", payload)
        return result

    def guardian_base_branches(self, guardian_id: str) -> list[str]:
        """List candidate base branches, scoped to the review's current remote."""
        result: list[str] = self._get(f"/api/guardians/{guardian_id}/base-branches")
        return result

    def guardian_change_base(self, guardian_id: str, branch: str) -> dict[str, Any]:
        """Change a review's base branch and trigger a rebuild."""
        result: dict[str, Any] = self._post(
            f"/api/guardians/{guardian_id}/base", {"branch": branch}
        )
        return result

    def guardian_force_start(self, guardian_id: str) -> dict[str, Any]:
        """Disable not-yet-done branches and kick off the merge immediately."""
        result: dict[str, Any] = self._post(f"/api/guardians/{guardian_id}/force_start")
        return result

    def guardian_dismiss_reenable(self, guardian_id: str, branch_id: str) -> dict[str, Any]:
        """Permanently dismiss the 're-enable' notification for a branch."""
        result: dict[str, Any] = self._post(
            f"/api/guardians/{guardian_id}/branches/{branch_id}/dismiss_reenable"
        )
        return result

    def guardian_move_branch(
        self, guardian_id: str, branch_id: str, to_guardian_id: str
    ) -> dict[str, Any]:
        """Move a branch to another review (RAL-118), then rebuild both.

        A move, not a copy: the branch leaves ``guardian_id``'s stack and is
        appended to ``to_guardian_id``'s. Raises `DaemonError` (409) if either
        review has a merge/rebase in flight.
        """
        result: dict[str, Any] = self._post(
            f"/api/guardians/{guardian_id}/branches/{branch_id}/move",
            {"to_guardian_id": to_guardian_id},
        )
        return result

    def guardian_merge(self, guardian_id: str) -> dict[str, Any]:
        """Start (or continue) the stacked-rebase merge."""
        result: dict[str, Any] = self._post(f"/api/guardians/{guardian_id}/merge")
        return result

    def guardian_cancel_and_merge(self, guardian_id: str) -> dict[str, Any]:
        """Cancel an in-progress rebase and immediately start a fresh one."""
        result: dict[str, Any] = self._post(f"/api/guardians/{guardian_id}/cancel_and_merge")
        return result

    def guardian_approve(self, guardian_id: str) -> dict[str, Any]:
        """Approve a review that is in_review."""
        result: dict[str, Any] = self._post(f"/api/guardians/{guardian_id}/approve")
        return result

    def guardian_cancel(self, guardian_id: str) -> dict[str, Any]:
        """Cancel a review."""
        result: dict[str, Any] = self._post(f"/api/guardians/{guardian_id}/cancel")
        return result

    # ── pull requests (RAL-117) ──────────────────────────────────────────

    def guardian_submit_prs(self, guardian_id: str, prs: list[dict[str, Any]]) -> dict[str, Any]:
        """Submit one or more PRs/MRs for a review's stacked/combined worktree(s).

        Each item in ``prs`` may set ``branch_id`` (omit for the combined
        worktree), ``branch_alias``, ``title``, and ``description``. Runs in the
        background on the daemon; poll `guardian_list_prs` for the result.
        """
        result: dict[str, Any] = self._post(
            f"/api/guardians/{guardian_id}/pull-requests", {"prs": prs}
        )
        return result

    def guardian_list_prs(self, guardian_id: str) -> list[dict[str, Any]]:
        """List every PR/MR submitted for a review, oldest first."""
        result: list[dict[str, Any]] = self._get(f"/api/guardians/{guardian_id}/pull-requests")
        return result

    def pr_get(self, pr_id: str) -> dict[str, Any]:
        """Fetch one PR row by its ralphus-internal id."""
        result: dict[str, Any] = self._get(f"/api/pull-requests/{pr_id}")
        return result

    def pr_find(self, forge: str, repo: str, pr_number: int) -> dict[str, Any]:
        """Look up the ralphus PR row for a given forge PR/MR number."""
        query = urlencode({"forge": forge, "repo": repo, "pr_number": pr_number})
        result: dict[str, Any] = self._get(f"/api/pull-requests?{query}")
        return result

    def pr_update(
        self,
        pr_id: str,
        *,
        pr_number: int | None = None,
        pr_url: str | None = None,
        branch_alias: str | None = None,
        state: str | None = None,
    ) -> dict[str, Any]:
        """Mutate the recorded PR mapping (e.g. after a PR is closed and reopened
        under a new number). Only present fields are changed."""
        payload: dict[str, Any] = {}
        for key, value in (
            ("pr_number", pr_number),
            ("pr_url", pr_url),
            ("branch_alias", branch_alias),
            ("state", state),
        ):
            if value is not None:
                payload[key] = value
        result: dict[str, Any] = self._post(f"/api/pull-requests/{pr_id}", payload)
        return result

    def pr_comments(self, pr_id: str) -> list[dict[str, Any]]:
        """Live-query the forge for a PR's comments, flagging which are already actioned."""
        result: list[dict[str, Any]] = self._get(f"/api/pull-requests/{pr_id}/comments")
        return result

    def pr_action_feedback(self, pr_id: str) -> dict[str, Any]:
        """Action a PR's un-actioned feedback into the owning review worktree."""
        result: dict[str, Any] = self._post(f"/api/pull-requests/{pr_id}/action-feedback")
        return result


def _extract_error_message(body: Any) -> str | None:
    if isinstance(body, dict):
        error = body.get("error")
        if isinstance(error, dict):
            message = error.get("message")
            if isinstance(message, str):
                return message
    return None
