"""Fake board-API payloads for docs screenshot generation.

Every helper here returns plain JSON-shaped dicts matching the daemon's real
response shapes — see ``docs/daemon-api.md``, ``daemon/src/store.rs``
(``RunView``/``TaskView``/``SessionView``/``VerifyView``/``QueueItem``) and
``daemon/src/guardian.rs`` (``GuardianView``/``BranchView``/``MessageView``).
No daemon, SQLite, or scheduler is involved, so the same module always
produces the same bytes.

Each ``*_ROUTES`` dict at the bottom maps an exact request path to the JSON
body ``stub_server`` should serve for it; ``shots.py`` picks a scenario by
name and starts a stub server bound to its routes.
"""

from __future__ import annotations

from typing import Any

__all__ = [
    "PATH_DEPLOY",
    "PATH_MIGRATE",
    "PATH_PROVISION",
    "PATH_PURGE",
    "QUEUE_ITEMS",
    "QUEUE_ROUTES",
    "QUEUE_RUN_ID",
    "QUEUE_RUN_LABEL",
    "RESOURCES_ROUTES",
    "RESOURCES_ROWS",
    "REVIEWS_BRANCHES",
    "REVIEWS_GUARDIAN",
    "REVIEWS_MESSAGES",
    "REVIEWS_ROUTES",
    "TASKS_ROUTES",
    "TASKS_RUNS",
    "Json",
    "Routes",
    "branch",
    "guardian",
    "message",
    "queue_item",
    "resource_row",
    "run",
    "session",
    "task",
    "verify_step",
]

Json = dict[str, Any]
Routes = dict[str, Any]  # route -> JSON body (dict or list)

# ---------------------------------------------------------------------------
# small builders, one per response shape
# ---------------------------------------------------------------------------


def verify_step(
    id_: str | None,
    kind: str,
    state: str,
    output: str | None,
    spec: str,
    *,
    agent: str = "claude",
    model: str | None = None,
    tokens_in: int = 0,
    tokens_out: int = 0,
    cost_usd: float = 0.0,
) -> Json:
    return {
        "id": id_,
        "kind": kind,
        "state": state,
        "output": output,
        "spec": spec,
        "model": model,
        "agent": agent,
        "tokens_in": tokens_in,
        "tokens_out": tokens_out,
        "cost_usd": cost_usd,
    }


def session(
    id_: str,
    name: str,
    *,
    cwd: str,
    state: str,
    agent: str = "claude",
    model: str | None = None,
    prompt: str | None = None,
    command: str | None = None,
    tokens_in: int = 0,
    tokens_out: int = 0,
    cost_usd: float = 0.0,
    error: str | None = None,
    depends_on: tuple[str, ...] = (),
    verify: tuple[Json, ...] = (),
) -> Json:
    return {
        "id": id_,
        "name": name,
        "cwd": cwd,
        "agent": agent,
        "model": model,
        "prompt": prompt,
        "command": command,
        "state": state,
        "tokens_in": tokens_in,
        "tokens_out": tokens_out,
        "cost_usd": cost_usd,
        "error": error,
        "depends_on": list(depends_on),
        "verify": list(verify),
        "reviews": [],
    }


def task(
    name: str,
    *,
    state: str,
    project: str,
    sessions: tuple[Json, ...] = (),
    verify: tuple[Json, ...] = (),
    depends_on: tuple[str, ...] = (),
) -> Json:
    return {
        "name": name,
        "project": project,
        "state": state,
        "sessions": list(sessions),
        "verify": list(verify),
        "depends_on": list(depends_on),
    }


def run(
    id_: str,
    *,
    state: str,
    created_at_ms: int,
    label: str | None = None,
    tasks: tuple[Json, ...] = (),
) -> Json:
    return {
        "id": id_,
        "label": label,
        "state": state,
        "created_at_ms": created_at_ms,
        "tasks": list(tasks),
        "reviews": [],
    }


def branch(
    position: int,
    branch_name: str,
    *,
    merge_status: str,
    enabled: bool = True,
    detail: str | None = None,
    worktree: str | None = None,
    conflicts_found: int | None = None,
    conflicts_fixed: int | None = None,
    conflicts_committed: int | None = None,
    project: str | None = None,
    source_session_state: str | None = None,
    can_reenable: bool = False,
    source_run_id: str | None = None,
    source_task_idx: int | None = None,
    source_session_idx: int | None = None,
) -> Json:
    return {
        "position": position,
        "branch": branch_name,
        "merge_status": merge_status,
        "detail": detail,
        "review_branch": f"review/{branch_name}",
        "worktree": worktree,
        "conflicts_found": conflicts_found,
        "conflicts_fixed": conflicts_fixed,
        "conflicts_committed": conflicts_committed,
        "enabled": enabled,
        "project": project,
        "source_session_state": source_session_state,
        "can_reenable": can_reenable,
        "source_run_id": source_run_id,
        "source_task_idx": source_task_idx,
        "source_session_idx": source_session_idx,
        "resolver_agent_session_id": None,
    }


def guardian(
    id_: str,
    name: str,
    *,
    base_branch: str,
    git_root: str,
    status: str,
    created_at_ms: int,
    base_commit: str | None = None,
    review_branch: str | None = None,
    detail: str | None = None,
    run_id: str | None = None,
    combined_worktree: str | None = None,
    skip_auto_build: bool = False,
    skip_worktree_checks: bool = False,
    skip_worktrees: bool = False,
    resolver_agent: str | None = "ollama",
    resolver_model: str | None = "qwen3:8b",
    checks: tuple[str, ...] = (),
    branches: tuple[Json, ...] = (),
    change_summary: str | None = None,
    manual_commands: tuple[str, ...] = (),
) -> Json:
    return {
        "id": id_,
        "name": name,
        "base_branch": base_branch,
        "base_commit": base_commit,
        "git_root": git_root,
        "review_branch": review_branch,
        "status": status,
        "detail": detail,
        "run_id": run_id,
        "combined_worktree": combined_worktree,
        "conflicts_found": None,
        "conflicts_fixed": None,
        "conflicts_committed": None,
        "skip_auto_build": skip_auto_build,
        "skip_worktree_checks": skip_worktree_checks,
        "review_type": "git",
        "skip_worktrees": skip_worktrees,
        "resolver_agent": resolver_agent,
        "resolver_model": resolver_model,
        "created_at_ms": created_at_ms,
        "checks": list(checks),
        "branches": list(branches),
        "change_summary": change_summary,
        "projects": [git_root],
        "base_commits": {git_root: base_commit} if base_commit else {},
        # RAL-164: manual_commands/action_hints are structured GuardianChecks
        # ({command, prompt, label, cleanup_command, inputs}), not bare
        # strings -- wrap each example command string into that shape so
        # generated docs show the real wire format.
        "manual_commands": [
            {"command": cmd, "prompt": None, "label": None, "cleanup_command": None, "inputs": []}
            for cmd in manual_commands
        ],
        "action_hints": [],
        "input_values": {},
        "input_resolutions": {},
        "summary_agent": resolver_agent,
        "summary_model": resolver_model,
        "manual_commands_agent": resolver_agent,
        "manual_commands_model": resolver_model,
        "chat_agent": resolver_agent,
        "chat_model": resolver_model,
        "squash_projects": [],
    }


def message(seq: int, role: str, text: str, at_ms: int) -> Json:
    return {"seq": seq, "role": role, "text": text, "at_ms": at_ms}


def queue_item(
    path: str,
    name: str,
    *,
    run_id: str,
    run_label: str | None,
    run_state: str,
    run_created_at_ms: int,
    kind: str,
    indent: int,
    task_idx: int,
    task_name: str,
    state: str,
    readiness: str,
    session_idx: int = -1,
    verify_idx: int = -1,
    verify_scope: str = "",
    blocked_by: tuple[str, ...] = (),
    task_depends_on: tuple[str, ...] = (),
    depends_on: tuple[str, ...] = (),
    deps_paths: tuple[str, ...] = (),
) -> Json:
    return {
        "run_id": run_id,
        "run_label": run_label,
        "run_state": run_state,
        "run_created_at_ms": run_created_at_ms,
        "kind": kind,
        "path": path,
        "indent": indent,
        "task_idx": task_idx,
        "task_name": task_name,
        "session_idx": session_idx,
        "verify_idx": verify_idx,
        "verify_scope": verify_scope,
        "name": name,
        "state": state,
        "readiness": readiness,
        "blocked_by": list(blocked_by),
        "task_depends_on": list(task_depends_on),
        "depends_on": list(depends_on),
        "deps_paths": list(deps_paths),
        "queue_rank": None,
    }


def resource_row(
    *,
    run_id: str,
    run_label: str | None,
    task_idx: int,
    task_name: str,
    session_idx: int,
    session_id: str,
    pid: int,
    cpu_percent: float | None,
    mem_bytes: int | None,
    gpu_mem_bytes: int | None,
) -> Json:
    return {
        "run_id": run_id,
        "run_label": run_label,
        "task_idx": task_idx,
        "task_name": task_name,
        "session_idx": session_idx,
        "session_id": session_id,
        "pid": pid,
        "cpu_percent": cpu_percent,
        "mem_bytes": mem_bytes,
        "gpu_mem_bytes": gpu_mem_bytes,
    }


def _daemon_status(running: int = 0, max_concurrent: int = 12) -> Json:
    return {"running": running, "max_concurrent": max_concurrent, "running_reviews": []}


def _empty_board() -> Json:
    """A minimal valid ``/api/tasks`` body — every tab's tick() hits this route."""
    return {"daemon": _daemon_status(), "runs": []}


# ---------------------------------------------------------------------------
# Tasks scenario — a board with runs spanning running/done/failed/queued
# ---------------------------------------------------------------------------

_REPO = "C:/repo/ralphus"

TASKS_RUNS: tuple[Json, ...] = (
    run(
        "run-000000000004",
        label="add-dark-mode-toggle",
        state="running",
        created_at_ms=1_783_130_000_000,
        tasks=(
            task(
                "frontend",
                project="librarian",
                state="running",
                verify=(
                    verify_step(
                        "lint", "command", "pending", None, "eslint librarian/assets/board.html"
                    ),
                ),
                sessions=(
                    session(
                        "session-0",
                        "wire-theme-toggle",
                        cwd=_REPO,
                        agent="claude",
                        model="claude-sonnet-5",
                        prompt="Add a dark/light theme toggle button to the board header.",
                        state="done",
                        tokens_in=8214,
                        tokens_out=1032,
                        cost_usd=0.041,
                        verify=(
                            verify_step(
                                "fmt", "command", "done", "ok", "prettier --check board.html"
                            ),
                        ),
                    ),
                    session(
                        "session-1",
                        "persist-theme-choice",
                        cwd=_REPO,
                        agent="claude",
                        model="claude-sonnet-5",
                        prompt="Persist the chosen theme to localStorage and restore it on load.",
                        state="running",
                        tokens_in=3021,
                        tokens_out=410,
                        cost_usd=0.014,
                        depends_on=("wire-theme-toggle",),
                    ),
                ),
            ),
        ),
    ),
    run(
        "run-000000000003",
        label="fix-flaky-queue-test",
        state="done",
        created_at_ms=1_783_120_000_000,
        tasks=(
            task(
                "backend",
                project="daemon",
                state="done",
                verify=(
                    verify_step(
                        "tests",
                        "command",
                        "done",
                        "31 passed",
                        "cargo test -p ralphus-daemon queue::",
                    ),
                ),
                sessions=(
                    session(
                        "session-0",
                        "stabilize-sort-tiebreak",
                        cwd=_REPO,
                        agent="claude",
                        model="claude-sonnet-5",
                        prompt="The queue sort order flips on ties — add a stable secondary key.",
                        state="done",
                        tokens_in=5502,
                        tokens_out=889,
                        cost_usd=0.028,
                        verify=(
                            verify_step(
                                "fmt", "command", "done", "ok", "cargo fmt --all -- --check"
                            ),
                        ),
                    ),
                ),
            ),
        ),
    ),
    run(
        "run-000000000002",
        label="refactor-auth-middleware",
        state="failed",
        created_at_ms=1_783_110_000_000,
        tasks=(
            task(
                "auth",
                project="daemon",
                state="failed",
                sessions=(
                    session(
                        "session-0",
                        "extract-token-parser",
                        cwd=_REPO,
                        agent="claude",
                        model="claude-sonnet-5",
                        command="cargo build -p ralphus-daemon",
                        state="failed",
                        error="error[E0502]: cannot borrow `store` as mutable more than once",
                        verify=(
                            verify_step(
                                "build",
                                "command",
                                "failed",
                                "error[E0502]: cannot borrow `store` as mutable more than once",
                                "cargo build -p ralphus-daemon",
                            ),
                        ),
                    ),
                ),
            ),
        ),
    ),
    run(
        "run-000000000001",
        label=None,
        state="queued",
        created_at_ms=1_783_100_000_000,
        tasks=(
            task(
                "docs",
                project="docs",
                state="pending",
                sessions=(
                    session(
                        "session-0",
                        "draft-installation-page",
                        cwd=_REPO,
                        agent="ollama",
                        model="qwen3:8b",
                        prompt="Write the installation doc page.",
                        state="pending",
                    ),
                ),
            ),
        ),
    ),
)

TASKS_ROUTES: Routes = {
    "/api/tasks": {"daemon": _daemon_status(running=1), "runs": list(TASKS_RUNS)},
    "/api/guardians": [],
    "/api/resources": {"resources": []},
    "/api/queue": {"items": []},
}

# ---------------------------------------------------------------------------
# Queue scenario — "run-migrations" depends on "provision-database". The
# initial order is already dependency-valid (provision, then migrate, then
# the unrelated cleanup task). Dragging "provision-database" down past its
# dependent and dropping it at the very end demonstrates lazy anchoring: the
# dependency lands exactly where dropped, and "run-migrations" is pushed down
# to stay after it (`queueAnchoredRepair` in librarian/assets/board.html).
# ---------------------------------------------------------------------------

QUEUE_RUN_ID = "run-000000000006"
QUEUE_RUN_LABEL = "launch-checklist"

PATH_PROVISION = f"{QUEUE_RUN_ID}/t0/s0"
PATH_DEPLOY = f"{QUEUE_RUN_ID}/t1/s0"
PATH_MIGRATE = f"{QUEUE_RUN_ID}/t0/s1"
PATH_PURGE = f"{QUEUE_RUN_ID}/t2/s0"

QUEUE_ITEMS: tuple[Json, ...] = (
    queue_item(
        PATH_PROVISION,
        "provision-database",
        run_id=QUEUE_RUN_ID,
        run_label=QUEUE_RUN_LABEL,
        run_state="pending",
        run_created_at_ms=1_783_140_000_000,
        kind="session",
        indent=2,
        task_idx=0,
        task_name="provisioning",
        session_idx=0,
        state="pending",
        readiness="ready",
    ),
    queue_item(
        PATH_DEPLOY,
        "deploy-canary",
        run_id=QUEUE_RUN_ID,
        run_label=QUEUE_RUN_LABEL,
        run_state="pending",
        run_created_at_ms=1_783_140_000_000,
        kind="session",
        indent=2,
        task_idx=1,
        task_name="rollout",
        session_idx=0,
        state="running",
        readiness="running",
        task_depends_on=("provisioning",),
    ),
    queue_item(
        PATH_MIGRATE,
        "run-migrations",
        run_id=QUEUE_RUN_ID,
        run_label=QUEUE_RUN_LABEL,
        run_state="pending",
        run_created_at_ms=1_783_140_000_000,
        kind="session",
        indent=2,
        task_idx=0,
        task_name="provisioning",
        session_idx=1,
        state="pending",
        readiness="blocked",
        blocked_by=("provision-database",),
        depends_on=("provision-database",),
        deps_paths=(PATH_PROVISION,),
    ),
    queue_item(
        PATH_PURGE,
        "purge-old-branch",
        run_id=QUEUE_RUN_ID,
        run_label=QUEUE_RUN_LABEL,
        run_state="pending",
        run_created_at_ms=1_783_140_000_000,
        kind="session",
        indent=2,
        task_idx=2,
        task_name="cleanup",
        session_idx=0,
        state="pending",
        readiness="excluded",
        blocked_by=("cancelled upstream session",),
    ),
)

QUEUE_ROUTES: Routes = {
    "/api/tasks": _empty_board(),
    "/api/guardians": [],
    "/api/resources": {"resources": []},
    "/api/queue": {"items": list(QUEUE_ITEMS)},
}

# ---------------------------------------------------------------------------
# Reviews scenario — one guardian with a reorderable/disable-able branch
# stack, a manual-checks dropdown, and a seeded feedback thread.
# ---------------------------------------------------------------------------

_GUARDIAN_ID = "guardian-000000000007"

REVIEWS_BRANCHES: tuple[Json, ...] = (
    branch(
        0,
        "task/provisioning",
        merge_status="done",
        worktree=f"{_REPO}/.git/.ralphus_guardian/{_GUARDIAN_ID}/0",
        project=_REPO,
        source_session_state="done",
        source_run_id=QUEUE_RUN_ID,
        source_task_idx=0,
        source_session_idx=0,
    ),
    branch(
        1,
        "task/rollout",
        merge_status="conflict_resolved",
        detail="resolved 2 conflicts in board.html",
        worktree=f"{_REPO}/.git/.ralphus_guardian/{_GUARDIAN_ID}/1",
        conflicts_found=2,
        conflicts_fixed=2,
        conflicts_committed=2,
        project=_REPO,
        source_session_state="done",
        source_run_id=QUEUE_RUN_ID,
        source_task_idx=1,
        source_session_idx=0,
    ),
    branch(
        2,
        "task/cleanup",
        merge_status="done",
        enabled=False,
        worktree=f"{_REPO}/.git/.ralphus_guardian/{_GUARDIAN_ID}/2",
        project=_REPO,
        source_session_state="done",
        source_run_id=QUEUE_RUN_ID,
        source_task_idx=2,
        source_session_idx=0,
    ),
)

REVIEWS_GUARDIAN: Json = guardian(
    _GUARDIAN_ID,
    "launch-checklist review",
    base_branch="master",
    base_commit="a1b2c3d",
    git_root=_REPO,
    review_branch="review/launch-checklist",
    status="in_review",
    run_id=QUEUE_RUN_ID,
    combined_worktree=f"{_REPO}/.git/.ralphus_guardian/{_GUARDIAN_ID}/combined",
    created_at_ms=1_783_141_000_000,
    checks=(
        "cargo test -p ralphus-daemon",
        "cargo clippy --all-targets -- -D warnings",
    ),
    branches=REVIEWS_BRANCHES,
    change_summary=(
        "Adds the Queue tab's drag-to-reorder priority list, provisions the "
        "launch-checklist database, and rolls out the canary deploy task."
    ),
    manual_commands=(
        "cargo run -p ralphus-daemon -- serve",
        "curl http://127.0.0.1:7890/api/queue | jq .",
    ),
)

REVIEWS_MESSAGES: tuple[Json, ...] = (
    message(
        1,
        "reviewer",
        "Please rename resolveConflict to resolveConflicts in task/rollout for "
        "consistency with the rest of the module.",
        1_783_141_500_000,
    ),
    message(
        2,
        "guardian",
        "Renamed resolveConflict → resolveConflicts in task/rollout, amended "
        "the commit, and pushed the update — the branch is up to date.",
        1_783_141_620_000,
    ),
)

REVIEWS_ROUTES: Routes = {
    "/api/tasks": _empty_board(),
    "/api/guardians": [REVIEWS_GUARDIAN],
    f"/api/guardians/{_GUARDIAN_ID}/messages": {"messages": list(REVIEWS_MESSAGES)},
    "/api/resources": {"resources": []},
    "/api/queue": {"items": []},
}

# ---------------------------------------------------------------------------
# Resources scenario — two running sessions with distinct CPU/RAM/GPU numbers
# ---------------------------------------------------------------------------

RESOURCES_ROWS: tuple[Json, ...] = (
    resource_row(
        run_id="run-000000000004",
        run_label="add-dark-mode-toggle",
        task_idx=0,
        task_name="frontend",
        session_idx=1,
        session_id="session-1",
        pid=51122,
        cpu_percent=12.5,
        mem_bytes=104_857_600,
        gpu_mem_bytes=None,
    ),
    resource_row(
        run_id=QUEUE_RUN_ID,
        run_label=QUEUE_RUN_LABEL,
        task_idx=1,
        task_name="rollout",
        session_idx=0,
        session_id="session-0",
        pid=48213,
        cpu_percent=142.7,
        mem_bytes=411_041_792,
        gpu_mem_bytes=2_147_483_648,
    ),
)

RESOURCES_ROUTES: Routes = {
    "/api/tasks": _empty_board(),
    "/api/guardians": [],
    "/api/resources": {"resources": list(RESOURCES_ROWS)},
    "/api/queue": {"items": []},
}
