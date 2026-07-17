"""Selector grammar: a single, path-shaped way to address any run/task/session/
verify or review/branch entity from the CLI (see CLI_PARITY_PLAN.local.md §2.2).

Grammar:
    <run_id>                                run
    <run_id>/<task>                         task node (task = index or name)
    <run_id>/<task>/<session>               session (session = index or name)
    <run_id>/<task>/verify/<i>              task-level verify step
    <run_id>/<task>/<session>/verify/<i>    session-level verify step
    <guardian_id> or @<name>                review (guardian)
    <guardian_id>#<pos-or-branch>           review branch

Parsing (`parse_run_selector` / `parse_guardian_selector`) never touches the
network -- it only splits the string and validates its shape, leaving name
segments unresolved. Resolution (`resolve_run_selector` /
`resolve_guardian_selector`) fetches the owning run/guardian view exactly once
and matches any name segment against it, raising `SelectorError` (mapped to
exit code 2) if a name is missing or ambiguous.
"""

from __future__ import annotations

from dataclasses import dataclass
from typing import TYPE_CHECKING, Any

if TYPE_CHECKING:
    from ralphus.client import DaemonClient

__all__ = [
    "RawRunSelector",
    "ResolvedGuardianSelector",
    "ResolvedSelector",
    "SelectorError",
    "parse_guardian_selector",
    "parse_run_selector",
    "resolve_guardian_selector",
    "resolve_run_selector",
]


class SelectorError(Exception):
    """A selector string could not be parsed or resolved. Maps to exit code 2."""


# ── run/task/session/verify selectors ────────────────────────────────────────


@dataclass(frozen=True)
class RawRunSelector:
    """An unresolved selector: name segments are raw strings, not yet matched
    to an index. ``task``/``session``/``verify`` are ``None`` when that level
    isn't addressed by the selector."""

    run_id: str
    task: str | None = None
    session: str | None = None
    verify_scope: str | None = None  # "task" | "session" | None
    verify: str | None = None


@dataclass(frozen=True)
class ResolvedSelector:
    """A fully resolved run/task/session/verify selector -- every name segment
    turned into its index. Mirrors the fields `DaemonClient.set_status` and the
    restart endpoints expect, so a resolved selector unpacks straight into them.
    """

    kind: str  # "run" | "task" | "session" | "verify"
    run_id: str
    task_idx: int = 0
    session_idx: int = -1
    verify_idx: int = -1
    verify_scope: str = ""


def parse_run_selector(raw: str) -> RawRunSelector:
    """Parse (but do not resolve) a run-family selector string."""
    segs = [s for s in raw.split("/") if s != ""]
    if not segs:
        raise SelectorError("empty selector")
    run_id, rest = segs[0], segs[1:]
    if not rest:
        return RawRunSelector(run_id)
    if len(rest) == 1:
        return RawRunSelector(run_id, task=rest[0])
    if len(rest) == 2:
        if rest[1] == "verify":
            raise SelectorError(f"'{raw}': 'verify' needs an index, e.g. .../verify/0")
        return RawRunSelector(run_id, task=rest[0], session=rest[1])
    if len(rest) == 3 and rest[1] == "verify":
        return RawRunSelector(run_id, task=rest[0], verify_scope="task", verify=rest[2])
    if len(rest) == 4 and rest[2] == "verify":
        return RawRunSelector(
            run_id, task=rest[0], session=rest[1], verify_scope="session", verify=rest[3]
        )
    raise SelectorError(f"cannot parse selector '{raw}'")


def _resolve_index(raw: str, *, candidates: list[str], what: str) -> int:
    """Resolve `raw` to an index into `candidates`: an int is used directly
    (bounds-checked); otherwise it's matched against `candidates` by name.
    """
    try:
        idx = int(raw)
    except ValueError:
        pass
    else:
        if not 0 <= idx < len(candidates):
            raise SelectorError(f"{what} index {idx} out of range (have {len(candidates)})")
        return idx
    matches = [i for i, name in enumerate(candidates) if name == raw]
    if not matches:
        options = ", ".join(candidates) or "(none)"
        raise SelectorError(f"no {what} named '{raw}' (available: {options})")
    if len(matches) > 1:
        raise SelectorError(
            f"'{raw}' matches {len(matches)} {what}s at positions {matches} -- use an index instead"
        )
    return matches[0]


def resolve_run_selector(client: DaemonClient, raw: str) -> ResolvedSelector:
    """Parse and resolve a run-family selector, fetching the run view once."""
    parsed = parse_run_selector(raw)
    if parsed.task is None:
        return ResolvedSelector(kind="run", run_id=parsed.run_id)

    run = client.run(parsed.run_id)
    tasks: list[dict[str, Any]] = run.get("tasks", [])
    task_names = [str(t.get("name", "")) for t in tasks]
    task_idx = _resolve_index(parsed.task, candidates=task_names, what="task")
    task = tasks[task_idx]

    if parsed.verify_scope == "task":
        assert parsed.verify is not None
        verify_idx = _resolve_index(
            parsed.verify,
            candidates=[str(i) for i in range(len(task.get("verify", [])))],
            what="task verify",
        )
        return ResolvedSelector(
            kind="verify",
            run_id=parsed.run_id,
            task_idx=task_idx,
            verify_idx=verify_idx,
            verify_scope="task",
        )

    if parsed.session is None:
        return ResolvedSelector(kind="task", run_id=parsed.run_id, task_idx=task_idx)

    sessions: list[dict[str, Any]] = task.get("sessions", [])
    session_names = [str(s.get("name") or s.get("id", "")) for s in sessions]
    session_idx = _resolve_index(parsed.session, candidates=session_names, what="session")

    if parsed.verify_scope == "session":
        assert parsed.verify is not None
        session = sessions[session_idx]
        verify_idx = _resolve_index(
            parsed.verify,
            candidates=[str(i) for i in range(len(session.get("verify", [])))],
            what="session verify",
        )
        return ResolvedSelector(
            kind="verify",
            run_id=parsed.run_id,
            task_idx=task_idx,
            session_idx=session_idx,
            verify_idx=verify_idx,
            verify_scope="session",
        )

    return ResolvedSelector(
        kind="session", run_id=parsed.run_id, task_idx=task_idx, session_idx=session_idx
    )


# ── review/branch selectors ───────────────────────────────────────────────────


@dataclass(frozen=True)
class ResolvedGuardianSelector:
    """A fully resolved review (guardian) or review-branch selector."""

    guardian_id: str
    # Stable branch id (RAL-122), None unless a branch was addressed. Callers
    # should send this to the daemon API, not a position -- position is a
    # display/reorder-only attribute that changes when the stack is reordered.
    branch_id: str | None = None
    branch: str | None = None


def parse_guardian_selector(raw: str) -> tuple[str, str | None]:
    """Split a guardian selector into ``(guardian_id_or_name, pos_or_branch)``.

    ``guardian_id_or_name`` is either a bare id, or ``@name`` (still unresolved
    at this point -- see `resolve_guardian_selector`).
    """
    if not raw:
        raise SelectorError("empty selector")
    head, sep, tail = raw.partition("#")
    if not head:
        raise SelectorError(f"cannot parse selector '{raw}'")
    return head, (tail if sep else None)


def resolve_guardian_selector(client: DaemonClient, raw: str) -> ResolvedGuardianSelector:
    """Parse and resolve a review/branch selector.

    Fetches the guardian list once if given ``@name``, and the guardian's own
    view once more if a branch (position or name) is addressed.
    """
    head, pos_or_branch = parse_guardian_selector(raw)

    guardian_id = head
    if head.startswith("@"):
        name = head[1:]
        guardians = client.guardian_list()
        matches = [g for g in guardians if g.get("name") == name]
        if not matches:
            options = ", ".join(str(g.get("name")) for g in guardians) or "(none)"
            raise SelectorError(f"no review named '{name}' (available: {options})")
        if len(matches) > 1:
            raise SelectorError(f"'{name}' matches {len(matches)} reviews -- use the id instead")
        guardian_id = str(matches[0]["id"])

    if pos_or_branch is None:
        return ResolvedGuardianSelector(guardian_id)

    guardian = client.guardian_get(guardian_id)
    branches: list[dict[str, Any]] = guardian.get("branches", [])
    try:
        position = int(pos_or_branch)
    except ValueError:
        matches = [b for b in branches if b.get("branch") == pos_or_branch]
        if not matches:
            options = ", ".join(str(b.get("branch")) for b in branches) or "(none)"
            raise SelectorError(
                f"no branch '{pos_or_branch}' in this review (available: {options})"
            ) from None
        if len(matches) > 1:
            raise SelectorError(
                f"'{pos_or_branch}' matches multiple branches -- use a position"
            ) from None
        branch_id = str(matches[0]["id"])
        branch_name = str(matches[0]["branch"])
    else:
        found = [b for b in branches if int(b.get("position", -1)) == position]
        if not found:
            raise SelectorError(f"no branch at position {position} in this review")
        branch_id = str(found[0]["id"])
        branch_name = str(found[0]["branch"])

    return ResolvedGuardianSelector(guardian_id, branch_id=branch_id, branch=branch_name)
