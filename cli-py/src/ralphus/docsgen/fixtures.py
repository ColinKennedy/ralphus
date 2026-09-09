"""Fake board-API payloads for docs screenshot generation.

Every helper here returns plain JSON-shaped dicts matching the daemon's real
response shapes — see ``docs/daemon-api.md``, ``daemon/src/store.rs``
(``SquadView``/``TaskView``/``CellView``/``ProofView``/``QueueItem``) and
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
    "CARTOGRAPHER_ROUTES",
    "CARTOGRAPHER_ROWS",
    "MACHINES_ROUTES",
    "MACHINES_ROWS",
    "PATH_DEPLOY",
    "PATH_MIGRATE",
    "PATH_PROVISION",
    "PATH_PURGE",
    "PREFS_HIDDEN_ITEMS",
    "PREFS_ROUTES",
    "PROJECTS_ROUTES",
    "PROJECTS_ROWS",
    "QUEUE_ITEMS",
    "QUEUE_ROUTES",
    "QUEUE_SQUAD_ID",
    "QUEUE_SQUAD_LABEL",
    "RESOURCES_ROUTES",
    "RESOURCES_ROWS",
    "REVIEWS_BRANCHES",
    "REVIEWS_GUARDIAN",
    "REVIEWS_MESSAGES",
    "REVIEWS_ROLLOUT_BRANCH_ID",
    "REVIEWS_ROUTES",
    "SECRETS_ROUTES",
    "SECRETS_ROWS",
    "TASKS_ROUTES",
    "TASKS_SQUADS",
    "TRIAGE_POOLS",
    "TRIAGE_ROUTES",
    "TRIAGE_SCHEDULES",
    "TRIAGE_TYPES",
    "USERS_ROUTES",
    "USERS_ROWS",
    "WORKTREE_RETIREMENT_ROUTES",
    "WORKTREE_RETIREMENT_ROWS",
    "Json",
    "Routes",
    "branch",
    "carto_row",
    "cell",
    "guardian",
    "hidden_item_entry",
    "machine_provider",
    "message",
    "project",
    "proof_step",
    "queue_item",
    "resource_row",
    "secret_env_name_entry",
    "squad",
    "task",
    "triage_pool_entry",
    "triage_schedule_entry",
    "triage_type_entry",
    "user_entry",
    "worktree_retirement_entry",
]

Json = dict[str, Any]
Routes = dict[str, Any]  # route -> JSON body (dict or list)

# ---------------------------------------------------------------------------
# small builders, one per response shape
# ---------------------------------------------------------------------------


def proof_step(
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


def cell(
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
    proof: tuple[Json, ...] = (),
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
        "proof": list(proof),
        "reviews": [],
    }


def task(
    name: str,
    *,
    state: str,
    project: str,
    cells: tuple[Json, ...] = (),
    proof: tuple[Json, ...] = (),
    depends_on: tuple[str, ...] = (),
) -> Json:
    return {
        "name": name,
        "project": project,
        "state": state,
        "cells": list(cells),
        "proof": list(proof),
        "depends_on": list(depends_on),
    }


def squad(
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
    id_: str,
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
    source_cell_state: str | None = None,
    can_reenable: bool = False,
    source_squad_id: str | None = None,
    source_task_idx: int | None = None,
    source_cell_idx: int | None = None,
) -> Json:
    return {
        "id": id_,
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
        "source_cell_state": source_cell_state,
        "can_reenable": can_reenable,
        "source_squad_id": source_squad_id,
        "source_task_idx": source_task_idx,
        "source_cell_idx": source_cell_idx,
        "source_cell_machine": None,
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
    squad_id: str | None = None,
    combined_worktree: str | None = None,
    skip_auto_build: bool = False,
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
        "squad_id": squad_id,
        "combined_worktree": combined_worktree,
        "conflicts_found": None,
        "conflicts_fixed": None,
        "conflicts_committed": None,
        "skip_auto_build": skip_auto_build,
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
    squad_id: str,
    squad_label: str | None,
    squad_state: str,
    squad_created_at_ms: int,
    kind: str,
    indent: int,
    task_idx: int,
    task_name: str,
    state: str,
    readiness: str,
    cell_idx: int = -1,
    proof_idx: int = -1,
    proof_scope: str = "",
    blocked_by: tuple[str, ...] = (),
    task_depends_on: tuple[str, ...] = (),
    depends_on: tuple[str, ...] = (),
    deps_paths: tuple[str, ...] = (),
) -> Json:
    return {
        "squad_id": squad_id,
        "squad_label": squad_label,
        "squad_state": squad_state,
        "squad_created_at_ms": squad_created_at_ms,
        "kind": kind,
        "path": path,
        "indent": indent,
        "task_idx": task_idx,
        "task_name": task_name,
        "cell_idx": cell_idx,
        "proof_idx": proof_idx,
        "proof_scope": proof_scope,
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
    squad_id: str,
    squad_label: str | None,
    task_idx: int,
    task_name: str,
    cell_idx: int,
    cell_id: str,
    pid: int,
    cpu_percent: float | None,
    mem_bytes: int | None,
    gpu_mem_bytes: int | None,
) -> Json:
    return {
        "squad_id": squad_id,
        "squad_label": squad_label,
        "task_idx": task_idx,
        "task_name": task_name,
        "cell_idx": cell_idx,
        "cell_id": cell_id,
        "pid": pid,
        "cpu_percent": cpu_percent,
        "mem_bytes": mem_bytes,
        "gpu_mem_bytes": gpu_mem_bytes,
    }


def carto_row(
    id_: int,
    *,
    at_ms: int,
    level: str,
    source: str,
    message: str,
    scope: str | None = None,
    squad_id: str | None = None,
    guardian_id: str | None = None,
    cell_id: str | None = None,
    task: str | None = None,
    payload: Json | None = None,
) -> Json:
    return {
        "id": id_,
        "at_ms": at_ms,
        "level": level,
        "source": source,
        "message": message,
        "scope": scope,
        "squad_id": squad_id,
        "guardian_id": guardian_id,
        "cell_id": cell_id,
        "task": task,
        "log_path": None,
        "payload": payload or {},
    }


def machine_provider(
    scheme: str,
    *,
    description: str,
    program: str,
    created_at_ms: int,
    args: tuple[str, ...] = (),
    protocol_version: int = 1,
    supports_channel: bool = False,
    last_check_ms: int | None = None,
    last_check_ok: bool | None = None,
    last_check_note: str | None = None,
) -> Json:
    return {
        "scheme": scheme,
        "description": description,
        "program": program,
        "args": list(args),
        "protocol_version": protocol_version,
        "created_at_ms": created_at_ms,
        "last_check_ms": last_check_ms,
        "last_check_ok": last_check_ok,
        "last_check_note": last_check_note,
        "supports_channel": supports_channel,
    }


def user_entry(name: str, *, created_at_ms: int) -> Json:
    return {"name": name, "created_at_ms": created_at_ms}


def secret_env_name_entry(name: str, *, created_at_ms: int) -> Json:
    return {"name": name, "created_at_ms": created_at_ms}


def triage_type_entry(name: str, *, label: str, description: str, created_at_ms: int) -> Json:
    return {
        "name": name,
        "label": label,
        "description": description,
        "created_at_ms": created_at_ms,
    }


def triage_pool_entry(
    project_name: str, triage_type: str, *, count: int, threshold: int | None
) -> Json:
    return {
        "project": project_name,
        "triage_type": triage_type,
        "count": count,
        "threshold": threshold,
    }


def triage_schedule_entry(
    id_: int,
    *,
    project_name: str,
    triage_type: str,
    cron_expr: str,
    anchor_date_ms: int,
    every_n: int,
    occurrence_count: int,
    last_checked_ms: int | None,
) -> Json:
    return {
        "id": id_,
        "project": project_name,
        "triage_type": triage_type,
        "cron_expr": cron_expr,
        "anchor_date_ms": anchor_date_ms,
        "every_n": every_n,
        "occurrence_count": occurrence_count,
        "last_checked_ms": last_checked_ms,
    }


def worktree_retirement_entry(
    guardian_id: str,
    guardian_name: str,
    *,
    project_root: str,
    path: str,
    state: str,
    eligible_at_ms: int,
    last_activity_ms: int | None = None,
    claim_kind: str | None = None,
    claim_owner: str | None = None,
    claim_state: str | None = None,
    error: str | None = None,
    last_attempt_ms: int | None = None,
    retry_at_ms: int | None = None,
) -> Json:
    return {
        "guardian_id": guardian_id,
        "guardian_name": guardian_name,
        "project_root": project_root,
        "path": path,
        "state": state,
        "eligible_at_ms": eligible_at_ms,
        "last_activity_ms": last_activity_ms,
        "claim_kind": claim_kind,
        "claim_owner": claim_owner,
        "claim_state": claim_state,
        "error": error,
        "last_attempt_ms": last_attempt_ms,
        "retry_at_ms": retry_at_ms,
    }


def hidden_item_entry(
    kind: str,
    *,
    squad_id: str | None = None,
    guardian_id: str | None = None,
    hidden_at_ms: int,
) -> Json:
    return {
        "kind": kind,
        "squad_id": squad_id,
        "guardian_id": guardian_id,
        "hidden_at_ms": hidden_at_ms,
    }


def project(
    name: str,
    *,
    description: str,
    path: str,
    created_at_ms: int,
    vcs: str = "git",
) -> Json:
    return {
        "name": name,
        "description": description,
        "path": path,
        "vcs": vcs,
        "created_at_ms": created_at_ms,
    }


def _daemon_status(running: int = 0, max_concurrent: int = 12) -> Json:
    return {"running": running, "max_concurrent": max_concurrent, "running_reviews": []}


def _empty_board() -> Json:
    """A minimal valid ``/api/tasks`` body — every tab's tick() hits this route."""
    return {"daemon": _daemon_status(), "squads": []}


# ---------------------------------------------------------------------------
# Tasks scenario — a board with squads spanning running/done/failed/queued
# ---------------------------------------------------------------------------

_REPO = "C:/repo/ralphus"

TASKS_SQUADS: tuple[Json, ...] = (
    squad(
        "squad-000000000004",
        label="add-dark-mode-toggle",
        state="running",
        created_at_ms=1_783_130_000_000,
        tasks=(
            task(
                "frontend",
                project="librarian",
                state="running",
                proof=(
                    proof_step(
                        "lint", "command", "pending", None, "eslint librarian/assets/board.html"
                    ),
                ),
                cells=(
                    cell(
                        "cell-0",
                        "wire-theme-toggle",
                        cwd=_REPO,
                        agent="claude",
                        model="claude-sonnet-5",
                        prompt="Add a dark/light theme toggle button to the board header.",
                        state="done",
                        tokens_in=8214,
                        tokens_out=1032,
                        cost_usd=0.041,
                        proof=(
                            proof_step(
                                "fmt", "command", "done", "ok", "prettier --check board.html"
                            ),
                        ),
                    ),
                    cell(
                        "cell-1",
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
    squad(
        "squad-000000000003",
        label="fix-flaky-queue-test",
        state="done",
        created_at_ms=1_783_120_000_000,
        tasks=(
            task(
                "backend",
                project="daemon",
                state="done",
                proof=(
                    proof_step(
                        "tests",
                        "command",
                        "done",
                        "31 passed",
                        "cargo test -p ralphus-daemon queue::",
                    ),
                ),
                cells=(
                    cell(
                        "cell-0",
                        "stabilize-sort-tiebreak",
                        cwd=_REPO,
                        agent="claude",
                        model="claude-sonnet-5",
                        prompt="The queue sort order flips on ties — add a stable secondary key.",
                        state="done",
                        tokens_in=5502,
                        tokens_out=889,
                        cost_usd=0.028,
                        proof=(
                            proof_step(
                                "fmt", "command", "done", "ok", "cargo fmt --all -- --check"
                            ),
                        ),
                    ),
                ),
            ),
        ),
    ),
    squad(
        "squad-000000000002",
        label="refactor-auth-middleware",
        state="failed",
        created_at_ms=1_783_110_000_000,
        tasks=(
            task(
                "auth",
                project="daemon",
                state="failed",
                cells=(
                    cell(
                        "cell-0",
                        "extract-token-parser",
                        cwd=_REPO,
                        agent="claude",
                        model="claude-sonnet-5",
                        command="cargo build -p ralphus-daemon",
                        state="failed",
                        error="error[E0502]: cannot borrow `store` as mutable more than once",
                        proof=(
                            proof_step(
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
    squad(
        "squad-000000000001",
        label=None,
        state="queued",
        created_at_ms=1_783_100_000_000,
        tasks=(
            task(
                "docs",
                project="docs",
                state="pending",
                cells=(
                    cell(
                        "cell-0",
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
    "/api/tasks": {"daemon": _daemon_status(running=1), "squads": list(TASKS_SQUADS)},
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

QUEUE_SQUAD_ID = "squad-000000000006"
QUEUE_SQUAD_LABEL = "launch-checklist"

PATH_PROVISION = f"{QUEUE_SQUAD_ID}/t0/s0"
PATH_DEPLOY = f"{QUEUE_SQUAD_ID}/t1/s0"
PATH_MIGRATE = f"{QUEUE_SQUAD_ID}/t0/s1"
PATH_PURGE = f"{QUEUE_SQUAD_ID}/t2/s0"

QUEUE_ITEMS: tuple[Json, ...] = (
    queue_item(
        PATH_PROVISION,
        "provision-database",
        squad_id=QUEUE_SQUAD_ID,
        squad_label=QUEUE_SQUAD_LABEL,
        squad_state="pending",
        squad_created_at_ms=1_783_140_000_000,
        kind="cell",
        indent=2,
        task_idx=0,
        task_name="provisioning",
        cell_idx=0,
        state="pending",
        readiness="ready",
    ),
    queue_item(
        PATH_DEPLOY,
        "deploy-canary",
        squad_id=QUEUE_SQUAD_ID,
        squad_label=QUEUE_SQUAD_LABEL,
        squad_state="pending",
        squad_created_at_ms=1_783_140_000_000,
        kind="cell",
        indent=2,
        task_idx=1,
        task_name="rollout",
        cell_idx=0,
        state="running",
        readiness="running",
        task_depends_on=("provisioning",),
    ),
    queue_item(
        PATH_MIGRATE,
        "run-migrations",
        squad_id=QUEUE_SQUAD_ID,
        squad_label=QUEUE_SQUAD_LABEL,
        squad_state="pending",
        squad_created_at_ms=1_783_140_000_000,
        kind="cell",
        indent=2,
        task_idx=0,
        task_name="provisioning",
        cell_idx=1,
        state="pending",
        readiness="blocked",
        blocked_by=("provision-database",),
        depends_on=("provision-database",),
        deps_paths=(PATH_PROVISION,),
    ),
    queue_item(
        PATH_PURGE,
        "purge-old-branch",
        squad_id=QUEUE_SQUAD_ID,
        squad_label=QUEUE_SQUAD_LABEL,
        squad_state="pending",
        squad_created_at_ms=1_783_140_000_000,
        kind="cell",
        indent=2,
        task_idx=2,
        task_name="cleanup",
        cell_idx=0,
        state="pending",
        readiness="excluded",
        blocked_by=("cancelled upstream cell",),
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

_BRANCH_PROVISIONING_ID = "branch-000000000001"
REVIEWS_ROLLOUT_BRANCH_ID = "branch-000000000002"
_BRANCH_CLEANUP_ID = "branch-000000000003"

REVIEWS_BRANCHES: tuple[Json, ...] = (
    branch(
        _BRANCH_PROVISIONING_ID,
        0,
        "task/provisioning",
        merge_status="done",
        worktree=f"{_REPO}/.git/.ralphus_guardian/{_GUARDIAN_ID}/0",
        project=_REPO,
        source_cell_state="done",
        source_squad_id=QUEUE_SQUAD_ID,
        source_task_idx=0,
        source_cell_idx=0,
    ),
    branch(
        REVIEWS_ROLLOUT_BRANCH_ID,
        1,
        "task/rollout",
        merge_status="conflict_resolved",
        detail="resolved 2 conflicts in board.html",
        worktree=f"{_REPO}/.git/.ralphus_guardian/{_GUARDIAN_ID}/1",
        conflicts_found=2,
        conflicts_fixed=2,
        conflicts_committed=2,
        project=_REPO,
        source_cell_state="done",
        source_squad_id=QUEUE_SQUAD_ID,
        source_task_idx=1,
        source_cell_idx=0,
    ),
    branch(
        _BRANCH_CLEANUP_ID,
        2,
        "task/cleanup",
        merge_status="done",
        enabled=False,
        worktree=f"{_REPO}/.git/.ralphus_guardian/{_GUARDIAN_ID}/2",
        project=_REPO,
        source_cell_state="done",
        source_squad_id=QUEUE_SQUAD_ID,
        source_task_idx=2,
        source_cell_idx=0,
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
    squad_id=QUEUE_SQUAD_ID,
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
    f"/api/guardians/{_GUARDIAN_ID}/branches/{REVIEWS_ROLLOUT_BRANCH_ID}/messages": {
        "messages": list(REVIEWS_MESSAGES)
    },
    "/api/resources": {"resources": []},
    "/api/queue": {"items": []},
}

# ---------------------------------------------------------------------------
# Resources scenario — two running sessions with distinct CPU/RAM/GPU numbers
# ---------------------------------------------------------------------------

RESOURCES_ROWS: tuple[Json, ...] = (
    resource_row(
        squad_id="squad-000000000004",
        squad_label="add-dark-mode-toggle",
        task_idx=0,
        task_name="frontend",
        cell_idx=1,
        cell_id="cell-1",
        pid=51122,
        cpu_percent=12.5,
        mem_bytes=104_857_600,
        gpu_mem_bytes=None,
    ),
    resource_row(
        squad_id=QUEUE_SQUAD_ID,
        squad_label=QUEUE_SQUAD_LABEL,
        task_idx=1,
        task_name="rollout",
        cell_idx=0,
        cell_id="cell-0",
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

# ---------------------------------------------------------------------------
# Cartographer scenario — a handful of events spanning info/warning/error
# levels and squad/cell/guardian scopes, newest first.
# ---------------------------------------------------------------------------

CARTOGRAPHER_ROWS: tuple[Json, ...] = (
    carto_row(
        1005,
        at_ms=1_783_142_500_000,
        level="info",
        source="runner",
        message="cell completed",
        scope="cell",
        squad_id="squad-000000000004",
        cell_id="cell-0",
        task="frontend",
        payload={"tokens_in": 8214, "tokens_out": 1032, "cost_usd": 0.041},
    ),
    carto_row(
        1004,
        at_ms=1_783_142_400_000,
        level="error",
        source="runner",
        message="cell extract-token-parser failed: cargo build exited 101",
        scope="cell",
        squad_id="squad-000000000002",
        cell_id="cell-0",
        task="auth",
    ),
    carto_row(
        1003,
        at_ms=1_783_142_300_000,
        level="warning",
        source="guardian_merge",
        message="resolved 2 conflicts in board.html",
        scope="guardian",
        guardian_id="guardian-000000000007",
    ),
    carto_row(
        1002,
        at_ms=1_783_142_200_000,
        level="info",
        source="runner",
        message="cell wire-theme-toggle started",
        scope="cell",
        squad_id="squad-000000000004",
        cell_id="cell-0",
        task="frontend",
    ),
    carto_row(
        1001,
        at_ms=1_783_142_100_000,
        level="info",
        source="scheduler",
        message="squad squad-000000000004 claimed → running",
        scope="squad",
        squad_id="squad-000000000004",
    ),
)

CARTOGRAPHER_ROUTES: Routes = {
    "/api/tasks": _empty_board(),
    "/api/guardians": [],
    "/api/resources": {"resources": []},
    "/api/queue": {"items": []},
    "/api/cartographer?limit=50&offset=0&sort=desc": {
        "rows": list(CARTOGRAPHER_ROWS),
        "total": len(CARTOGRAPHER_ROWS),
    },
}

# ---------------------------------------------------------------------------
# Machines scenario — one reachable, channel-capable provider and one that
# has never been probed.
# ---------------------------------------------------------------------------

MACHINES_ROWS: tuple[Json, ...] = (
    machine_provider(
        "incredibuild",
        description="on-prem build farm",
        program="/opt/ralphus/incredibuild.sh",
        created_at_ms=1_783_100_000_000,
        protocol_version=1,
        supports_channel=True,
        last_check_ms=1_783_142_000_000,
        last_check_ok=True,
        last_check_note="build farm responded to ping in 42ms",
    ),
    machine_provider(
        "lab-gpu-01",
        description="GPU workstation for local model runs",
        program="C:/ralphus/providers/ssh_provider.exe",
        created_at_ms=1_783_110_000_000,
        args=("--host", "lab-gpu-01"),
        protocol_version=1,
    ),
)

MACHINES_ROUTES: Routes = {
    "/api/tasks": _empty_board(),
    "/api/guardians": [],
    "/api/resources": {"resources": []},
    "/api/queue": {"items": []},
    "/api/machines": {"machines": list(MACHINES_ROWS), "builtin": ["local"]},
}

# ---------------------------------------------------------------------------
# Users scenario — a few placeholder identities.
# ---------------------------------------------------------------------------

USERS_ROWS: tuple[Json, ...] = (
    user_entry("ci-bot", created_at_ms=1_783_090_000_000),
    user_entry("colin", created_at_ms=1_783_095_000_000),
    user_entry("reviewer", created_at_ms=1_783_098_000_000),
)

USERS_ROUTES: Routes = {
    "/api/tasks": _empty_board(),
    "/api/guardians": [],
    "/api/resources": {"resources": []},
    "/api/queue": {"items": []},
    "/api/users": {"users": list(USERS_ROWS)},
}

# ---------------------------------------------------------------------------
# Secrets scenario — a few registered secret env-var names (RAL-281).
# ---------------------------------------------------------------------------

SECRETS_ROWS: tuple[Json, ...] = (
    secret_env_name_entry("STRIPE_SECRET_KEY", created_at_ms=1_783_100_000_000),
    secret_env_name_entry("DATABASE_URL", created_at_ms=1_783_104_000_000),
    secret_env_name_entry("GITHUB_TOKEN", created_at_ms=1_783_108_000_000),
)

SECRETS_ROUTES: Routes = {
    "/api/tasks": _empty_board(),
    "/api/guardians": [],
    "/api/resources": {"resources": []},
    "/api/queue": {"items": []},
    "/api/secret-env-names": {"names": list(SECRETS_ROWS)},
}

# ---------------------------------------------------------------------------
# Projects scenario — a few registered git repositories, all validating
# clean (RAL-101 re-checks each one's on-disk path once per tab-load).
# ---------------------------------------------------------------------------

PROJECTS_ROWS: tuple[Json, ...] = (
    project(
        "ralphus",
        description="the ralphus repo",
        path=_REPO,
        created_at_ms=1_783_080_000_000,
    ),
    project(
        "docs-site",
        description="internal docs site",
        path="C:/repo/docs-site",
        created_at_ms=1_783_082_000_000,
    ),
    project(
        "incredibuild-scripts",
        description="build farm provisioning scripts",
        path="C:/repo/incredibuild-scripts",
        created_at_ms=1_783_084_000_000,
    ),
)

PROJECTS_ROUTES: Routes = {
    "/api/tasks": _empty_board(),
    "/api/guardians": [],
    "/api/resources": {"resources": []},
    "/api/queue": {"items": []},
    "/api/projects": {"projects": list(PROJECTS_ROWS)},
    "/api/projects/ralphus/validate": {"valid": True},
    "/api/projects/docs-site/validate": {"valid": True},
    "/api/projects/incredibuild-scripts/validate": {"valid": True},
}

# ---------------------------------------------------------------------------
# Triage scenario (RAL-318) — two registered types, one pool close to its
# count threshold, one pool with only a cron schedule configured.
# ---------------------------------------------------------------------------

TRIAGE_TYPES: tuple[Json, ...] = (
    triage_type_entry(
        "unclassified",
        label="Unclassified",
        description="Built-in fallback when classification fails, times out, or is ambiguous.",
        created_at_ms=1_783_000_000_000,
    ),
    triage_type_entry(
        "security",
        label="Security",
        description="Changes touching auth, secrets handling, or attack surface.",
        created_at_ms=1_783_050_000_000,
    ),
    triage_type_entry(
        "docs",
        label="Docs",
        description="Documentation-only changes with no code behavior impact.",
        created_at_ms=1_783_060_000_000,
    ),
)

TRIAGE_POOLS: tuple[Json, ...] = (
    triage_pool_entry("ralphus", "security", count=4, threshold=5),
    triage_pool_entry("ralphus", "docs", count=2, threshold=None),
)

TRIAGE_SCHEDULES: tuple[Json, ...] = (
    triage_schedule_entry(
        1,
        project_name="ralphus",
        triage_type="docs",
        cron_expr="0 0 * * MON",
        anchor_date_ms=1_782_000_000_000,
        every_n=2,
        occurrence_count=3,
        last_checked_ms=1_783_142_000_000,
    ),
)

TRIAGE_ROUTES: Routes = {
    "/api/tasks": _empty_board(),
    "/api/guardians": [],
    "/api/resources": {"resources": []},
    "/api/queue": {"items": []},
    "/api/triage/types": {"types": list(TRIAGE_TYPES)},
    "/api/triage/pools": {"pools": list(TRIAGE_POOLS)},
    "/api/triage/schedules": {"schedules": list(TRIAGE_SCHEDULES)},
}

# ---------------------------------------------------------------------------
# Worktree retirement scenario (RAL-385/386) — one row per lifecycle state,
# admin-only tab.
# ---------------------------------------------------------------------------

WORKTREE_RETIREMENT_ROWS: tuple[Json, ...] = (
    worktree_retirement_entry(
        "guardian-000000000001",
        "add-rate-limiter",
        project_root=_REPO,
        path=f"{_REPO}-worktrees/add-rate-limiter-review",
        state="scheduled",
        eligible_at_ms=1_783_600_000_000,
    ),
    worktree_retirement_entry(
        "guardian-000000000002",
        "fix-flaky-scheduler-test",
        project_root=_REPO,
        path=f"{_REPO}-worktrees/fix-flaky-scheduler-test-review",
        state="eligible",
        eligible_at_ms=1_783_140_000_000,
    ),
    worktree_retirement_entry(
        "guardian-000000000003",
        "rework-guardian-merge",
        project_root=_REPO,
        path=f"{_REPO}-worktrees/rework-guardian-merge-review",
        state="claimed",
        eligible_at_ms=1_783_100_000_000,
        claim_kind="merge",
        claim_owner="colin",
        claim_state="running",
    ),
    worktree_retirement_entry(
        "guardian-000000000004",
        "migrate-secret-store",
        project_root=_REPO,
        path=f"{_REPO}-worktrees/migrate-secret-store-review",
        state="failed",
        eligible_at_ms=1_783_080_000_000,
        error="permission denied removing .git/worktrees/migrate-secret-store-review",
        last_attempt_ms=1_783_142_000_000,
    ),
    worktree_retirement_entry(
        "guardian-000000000005",
        "provision-lab-gpu-01",
        project_root=_REPO,
        path=f"{_REPO}-worktrees/provision-lab-gpu-01-review",
        state="deferred",
        eligible_at_ms=1_783_070_000_000,
        error="machine provider is mid-snapshot, try again later",
        last_attempt_ms=1_783_141_000_000,
        retry_at_ms=1_783_400_000_000,
    ),
    worktree_retirement_entry(
        "guardian-000000000006",
        "pin-incredibuild-agent",
        project_root=_REPO,
        path=f"{_REPO}-worktrees/pin-incredibuild-agent-review",
        state="opted_out",
        eligible_at_ms=1_783_060_000_000,
        error="static machine retirement policy excludes this provider",
        last_attempt_ms=1_783_140_000_000,
    ),
    worktree_retirement_entry(
        "guardian-000000000007",
        "retire-old-runner-shim",
        project_root=_REPO,
        path=f"{_REPO}-worktrees/retire-old-runner-shim-review",
        state="retired",
        eligible_at_ms=1_782_900_000_000,
        last_attempt_ms=1_782_950_000_000,
    ),
)

WORKTREE_RETIREMENT_ROUTES: Routes = {
    "/api/tasks": _empty_board(),
    "/api/guardians": [],
    "/api/resources": {"resources": []},
    "/api/queue": {"items": []},
    "/api/worktree-retirements": {"entries": list(WORKTREE_RETIREMENT_ROWS)},
}

# ---------------------------------------------------------------------------
# Preferences scenario (RAL-329) — a couple of hidden items in the current
# user's own view.
# ---------------------------------------------------------------------------

PREFS_HIDDEN_ITEMS: tuple[Json, ...] = (
    hidden_item_entry("squad", squad_id="squad-000000000002", hidden_at_ms=1_783_120_000_000),
    hidden_item_entry(
        "review", guardian_id="guardian-000000000007", hidden_at_ms=1_783_130_000_000
    ),
)

PREFS_ROUTES: Routes = {
    "/api/tasks": {"daemon": _daemon_status(), "squads": list(TASKS_SQUADS)},
    "/api/guardians": [REVIEWS_GUARDIAN],
    "/api/resources": {"resources": []},
    "/api/queue": {"items": []},
    "/api/hidden": {"hidden": list(PREFS_HIDDEN_ITEMS)},
}
