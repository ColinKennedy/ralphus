"""Selector grammar: a single, path-shaped way to address any run/task/session/
verify or review/branch entity from the CLI (see CLI_PARITY_PLAN.local.md §2.2).

**The ralphus URI scheme (RAL-188) is the grammar this module now produces.**
`ralphus.uri` owns its parsing; this module is where a URI is *resolved* to the
positional coordinates (`task_idx`/`session_idx`/`verify_idx`, branch ids) that
the daemon's REST API is built on::

    ralphus:/RUN[my run]/TASK[ral-178]/SESSION[work]?id=run-000000000151
    ralphus:/RUN[my run]/TASK[ral-178]/VERIFY[~0]?id=run-000000000151
    ralphus:/REVIEW[RAL-174/175 batch]?id=guardian-000000000003&worktree=~2

The **legacy** grammar below is still *accepted* -- per RAL-188 §C.6 it is an
alias kept inside this same chokepoint, exercised by the same tests, with
nowhere to drift to. Only the URI form is ever produced. Removing the alias is
a deliberate follow-up, not part of RAL-188::

    <run_id>                                run
    <run_id>/<task>                         task node (task = index or name)
    <run_id>/<task>/<session>               session (session = index or name)
    <run_id>/<task>/verify/<i>              task-level verify step
    <run_id>/<task>/<session>/verify/<i>    session-level verify step
    <guardian_id> or @<name>                review (guardian)
    <guardian_id>~<pos-or-branch>           review branch
    <guardian_id>#<pos-or-branch>           review branch (older spelling)

The two differ in exactly one resolution rule, and it is deliberate: in the
legacy grammar a bare integer means a *position*, while in the URI grammar a
position always carries the `URI_INDEX_SIGIL_TOKEN` sigil (``VERIFY[~0]``) and
a bare token is always a *name* (``VERIFY[0]`` is the step literally named
``0``). See `_resolve_index`.

The legacy review-branch separator is the same story: it was ``#``, RFC 3986's
fragment delimiter, which truncates the selector anywhere it is parsed as a
real URL. Both spellings are accepted (`BRANCH_SEPARATOR_TOKENS`); only the
URI form -- with ``~`` -- is ever produced.

Parsing (`parse_run_selector` / `parse_guardian_selector`) never touches the
network -- it only splits the string and validates its shape, leaving name
segments unresolved. Resolution (`resolve_run_selector` /
`resolve_guardian_selector`) fetches the owning run/guardian view and matches
any name segment against it, raising `SelectorError` (mapped to exit code 2) if
a name is missing or ambiguous. An ambiguous name is never silently
disambiguated -- the error lists the candidates and points at ``?id=``
(RAL-188 §C.3).
"""

from __future__ import annotations

from dataclasses import dataclass
from typing import TYPE_CHECKING, Any

from ralphus.uri import (
    URI_INDEX_SIGIL_TOKEN,
    RalphusUri,
    Segment,
    UriError,
    looks_like_uri,
    parse_uri,
    review_uri,
    run_uri,
)

if TYPE_CHECKING:
    from ralphus.client import DaemonClient

__all__ = [
    "BRANCH_SEPARATOR_TOKENS",
    "RawGuardianSelector",
    "RawRunSelector",
    "ResolvedGuardianSelector",
    "ResolvedSelector",
    "SelectorError",
    "guardian_view_uri",
    "parse_guardian_selector",
    "parse_run_selector",
    "resolve_guardian_selector",
    "resolve_run_selector",
    "run_view_uri",
]


class SelectorError(Exception):
    """A selector string could not be parsed or resolved. Maps to exit code 2."""


#: The legacy review-branch separator, kept for compatibility:
#: ``guardian-000000000003#2``. It is RFC 3986's fragment delimiter, so anything
#: that parses a selector as a real URL truncates it there.
_LEGACY_BRANCH_SEPARATOR_TOKEN = "#"

#: What `parse_guardian_selector` will split a legacy review-branch selector
#: on, preferred spelling first. `URI_INDEX_SIGIL_TOKEN` (``~``) is the
#: unreserved character the URI grammar uses; the legacy ``#`` still parses so
#: existing links, scripts, and shell history keep working.
BRANCH_SEPARATOR_TOKENS = (URI_INDEX_SIGIL_TOKEN, _LEGACY_BRANCH_SEPARATOR_TOKEN)


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
    # ── RAL-188 URI form only ────────────────────────────────────────────────
    #: The ``RUN[...]`` label. Only consulted when `run_id` is empty (the URI
    #: carried no ``?id=`` sidecar), in which case it is looked up against the
    #: run list -- matching either a run's label or its id, since a run with no
    #: label renders as its id (§C.2).
    run_label: str | None = None
    #: True when parsed from a URI: a ``~N`` token is a position and every
    #: other token is a name, never a position. See `_resolve_index`.
    uri_form: bool = False


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


def _segment_token(segment: Segment) -> str:
    """Render a parsed URI segment back to a single resolution token: ``~N``
    for a positional index, the decoded name otherwise."""
    if segment.index is not None:
        return f"{URI_INDEX_SIGIL_TOKEN}{segment.index}"
    return str(segment.name)


def _run_selector_from_uri(uri: RalphusUri, raw: str) -> RawRunSelector:
    """Convert a parsed run-family URI into a `RawRunSelector` (offline)."""
    if uri.kinds[0] != "RUN":
        raise SelectorError(f"'{raw}' addresses a review, not a run/task/session/verify")
    by_kind = {s.kind: s for s in uri.segments}
    run_seg = by_kind["RUN"]
    if run_seg.index is not None:
        raise SelectorError(
            f"'{raw}': RUN[{URI_INDEX_SIGIL_TOKEN}N] is not addressable -- a run has no "
            "stable position. Use its label or id (and ideally '?id=<run id>')."
        )
    task = by_kind.get("TASK")
    session = by_kind.get("SESSION")
    verify = by_kind.get("VERIFY")
    return RawRunSelector(
        run_id=uri.id or "",
        run_label=str(run_seg.name),
        uri_form=True,
        task=_segment_token(task) if task is not None else None,
        session=_segment_token(session) if session is not None else None,
        verify_scope=None if verify is None else ("session" if session is not None else "task"),
        verify=_segment_token(verify) if verify is not None else None,
    )


def parse_run_selector(raw: str) -> RawRunSelector:
    """Parse (but do not resolve) a run-family selector string.

    Accepts the RAL-188 URI form and the legacy path form; which one is in play
    is decided by `ralphus.uri.looks_like_uri`, so a legacy selector can never
    be mis-read as a malformed URI (no legacy selector contains a ``[``).
    """
    if looks_like_uri(raw):
        try:
            uri = parse_uri(raw)
        except UriError as exc:
            raise SelectorError(str(exc)) from exc
        return _run_selector_from_uri(uri, raw)

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


def _resolve_index(raw: str, *, candidates: list[str], what: str, uri_form: bool = False) -> int:
    """Resolve `raw` to an index into `candidates`.

    Legacy form: a bare int is used directly as a position (bounds-checked);
    anything else is matched by name. URI form (`uri_form`): only a ``~N``
    token is a position, so a bare ``0`` addresses the entity *named* ``0``
    (RAL-188 §C.4) and a name can never be swallowed by an index.
    """
    sigil = URI_INDEX_SIGIL_TOKEN
    positional = raw[len(sigil) :] if raw.startswith(sigil) else (None if uri_form else raw)
    if positional is not None:
        try:
            idx = int(positional)
        except ValueError:
            if uri_form:
                raise SelectorError(
                    f"{what} '{raw}': '{sigil}' must be followed by a number"
                ) from None
        else:
            if not 0 <= idx < len(candidates):
                raise SelectorError(f"{what} index {idx} out of range (have {len(candidates)})")
            return idx
    # An entity with no name of its own contributes an empty candidate; it is
    # only addressable positionally, so never let it match a name lookup.
    matches = [i for i, name in enumerate(candidates) if name and name == raw]
    if not matches:
        options = ", ".join(c or sigil + str(i) for i, c in enumerate(candidates)) or "(none)"
        raise SelectorError(f"no {what} named '{raw}' (available: {options})")
    if len(matches) > 1:
        hint = (
            f"use {sigil}{matches[0]} (a positional index) instead" if uri_form else "use an index"
        )
        raise SelectorError(
            f"'{raw}' matches {len(matches)} {what}s at positions {matches} -- {hint}"
        )
    return matches[0]


def _verify_candidates(steps: list[dict[str, Any]]) -> list[str]:
    """Addressable names for a list of verify steps: the author-supplied
    ``id``, or empty for an anonymous step (addressable only as ``~N`` --
    RAL-188 §C.4)."""
    return [str(s.get("id") or "") for s in steps]


def _lookup_run_id(client: DaemonClient, parsed: RawRunSelector) -> str:
    """Resolve a URI's ``RUN[label]`` to a run id when it carried no ``?id=``.

    Matches a run's label *or* its id, since a run with no label renders as its
    id (§C.2). Ambiguity is an error listing the candidates -- never a silent
    "most recent wins" (§C.3).
    """
    label = parsed.run_label
    if not label:
        raise SelectorError("selector does not name a run")
    runs: list[dict[str, Any]] = client.tasks().get("runs", [])
    matches = [r for r in runs if label in (str(r.get("label") or ""), str(r.get("id") or ""))]
    if not matches:
        raise SelectorError(f"no run labelled '{label}'")
    if len(matches) > 1:
        ids = ", ".join(str(r.get("id")) for r in matches)
        raise SelectorError(
            f"'{label}' matches {len(matches)} runs ({ids}) -- add '?id=<run id>' to say which one"
        )
    return str(matches[0].get("id"))


def resolve_run_selector(client: DaemonClient, raw: str) -> ResolvedSelector:
    """Parse and resolve a run-family selector, fetching the run view once."""
    parsed = parse_run_selector(raw)
    run_id = parsed.run_id or _lookup_run_id(client, parsed)
    if parsed.task is None:
        return ResolvedSelector(kind="run", run_id=run_id)

    run = client.run(run_id)
    tasks: list[dict[str, Any]] = run.get("tasks", [])
    task_names = [str(t.get("name", "")) for t in tasks]
    task_idx = _resolve_index(
        parsed.task, candidates=task_names, what="task", uri_form=parsed.uri_form
    )
    task = tasks[task_idx]

    if parsed.verify_scope == "task":
        assert parsed.verify is not None
        verify_idx = _resolve_index(
            parsed.verify,
            candidates=_verify_candidates(task.get("verify", [])),
            what="task verify",
            uri_form=parsed.uri_form,
        )
        return ResolvedSelector(
            kind="verify",
            run_id=run_id,
            task_idx=task_idx,
            verify_idx=verify_idx,
            verify_scope="task",
        )

    if parsed.session is None:
        return ResolvedSelector(kind="task", run_id=run_id, task_idx=task_idx)

    sessions: list[dict[str, Any]] = task.get("sessions", [])
    session_names = [str(s.get("name") or s.get("id", "")) for s in sessions]
    session_idx = _resolve_index(
        parsed.session, candidates=session_names, what="session", uri_form=parsed.uri_form
    )

    if parsed.verify_scope == "session":
        assert parsed.verify is not None
        session = sessions[session_idx]
        verify_idx = _resolve_index(
            parsed.verify,
            candidates=_verify_candidates(session.get("verify", [])),
            what="session verify",
            uri_form=parsed.uri_form,
        )
        return ResolvedSelector(
            kind="verify",
            run_id=run_id,
            task_idx=task_idx,
            session_idx=session_idx,
            verify_idx=verify_idx,
            verify_scope="session",
        )

    return ResolvedSelector(
        kind="session", run_id=run_id, task_idx=task_idx, session_idx=session_idx
    )


# ── review/branch selectors ───────────────────────────────────────────────────


@dataclass(frozen=True)
class RawGuardianSelector:
    """An unresolved review selector."""

    #: A guardian id, an ``@name``, or (URI form) the ``REVIEW[...]`` label.
    head: str
    #: A branch's label (its feature branch name), its stable ``branch-...``
    #: id, or a position -- ``~N`` for a position in the URI form.
    branch: str | None = None
    #: ``?combined`` -- the review's combined worktree rather than one branch.
    combined: bool = False
    #: The authoritative ``?id=`` sidecar, when the URI carried one.
    guardian_id: str | None = None
    #: True when parsed from a URI (see `RawRunSelector.uri_form`).
    uri_form: bool = False


@dataclass(frozen=True)
class ResolvedGuardianSelector:
    """A fully resolved review (guardian) or review-branch selector."""

    guardian_id: str
    # Stable branch id (RAL-122), None unless a branch was addressed. Callers
    # should send this to the daemon API, not a position -- position is a
    # display/reorder-only attribute that changes when the stack is reordered.
    branch_id: str | None = None
    branch: str | None = None
    #: The selector addressed the review's combined worktree (``?combined``),
    #: not a single branch. Mutually exclusive with `branch_id` (RAL-188 §C.1).
    combined: bool = False


def _split_legacy_branch(raw: str) -> tuple[str, str | None]:
    """Split a legacy review selector into ``(head, branch-or-None)`` at the
    earliest `BRANCH_SEPARATOR_TOKENS` occurrence.

    Earliest-wins rather than preferred-token-wins, so a branch name that
    itself contains the other separator still splits where the user meant it
    to: ``@my review#fix~1`` is branch ``fix~1``, not head ``@my review#fix``.
    """
    cuts = [at for at in (raw.find(token) for token in BRANCH_SEPARATOR_TOKENS) if at >= 0]
    if not cuts:
        return raw, None
    at = min(cuts)
    return raw[:at], raw[at + 1 :]


def parse_guardian_selector(raw: str) -> RawGuardianSelector:
    """Parse (but do not resolve) a review selector.

    Accepts the RAL-188 ``REVIEW[...]`` URI form and the legacy
    ``<id-or-@name>[~<pos-or-branch>]`` form (whose separator was originally
    ``#`` -- both spellings still parse, see `BRANCH_SEPARATOR_TOKENS`).
    """
    if looks_like_uri(raw):
        try:
            uri = parse_uri(raw)
        except UriError as exc:
            raise SelectorError(str(exc)) from exc
        if uri.kinds[0] != "REVIEW":
            raise SelectorError(f"'{raw}' addresses a {uri.kinds[0].lower()}, not a review")
        segment = uri.segments[0]
        if segment.index is not None:
            raise SelectorError(
                f"'{raw}': REVIEW[{URI_INDEX_SIGIL_TOKEN}N] is not addressable -- a review "
                "has no stable position. Use its name or id (and ideally "
                "'?id=<guardian id>')."
            )
        return RawGuardianSelector(
            head=str(segment.name),
            branch=uri.get("worktree"),
            combined=uri.has("combined"),
            guardian_id=uri.id,
            uri_form=True,
        )

    if not raw:
        raise SelectorError("empty selector")
    head, branch = _split_legacy_branch(raw)
    if not head:
        raise SelectorError(f"cannot parse selector '{raw}'")
    return RawGuardianSelector(head=head, branch=branch)


def _lookup_guardian_id(client: DaemonClient, parsed: RawGuardianSelector) -> str:
    """Resolve a review selector's head to a guardian id.

    ``?id=`` wins outright when present (§C.3). Otherwise an ``@name`` (legacy)
    matches by name, and a URI label matches a review's name *or* its id.
    """
    if parsed.guardian_id:
        return parsed.guardian_id
    if not parsed.uri_form and not parsed.head.startswith("@"):
        return parsed.head

    name = parsed.head[1:] if parsed.head.startswith("@") else parsed.head
    guardians = client.guardian_list()
    matches = [
        g
        for g in guardians
        if g.get("name") == name or (parsed.uri_form and str(g.get("id")) == name)
    ]
    if not matches:
        options = ", ".join(str(g.get("name")) for g in guardians) or "(none)"
        raise SelectorError(f"no review named '{name}' (available: {options})")
    if len(matches) > 1:
        ids = ", ".join(str(g.get("id")) for g in matches)
        hint = (
            f"add '?id=<guardian id>' to say which one ({ids})"
            if parsed.uri_form
            else "use the id instead"
        )
        raise SelectorError(f"'{name}' matches {len(matches)} reviews -- {hint}")
    return str(matches[0]["id"])


def resolve_guardian_selector(client: DaemonClient, raw: str) -> ResolvedGuardianSelector:
    """Parse and resolve a review/branch selector.

    Fetches the guardian list once if the review is addressed by name, and the
    guardian's own view once more if a branch (position or name) is addressed.
    """
    parsed = parse_guardian_selector(raw)
    guardian_id = _lookup_guardian_id(client, parsed)

    if parsed.branch is None:
        return ResolvedGuardianSelector(guardian_id, combined=parsed.combined)

    guardian = client.guardian_get(guardian_id)
    branches: list[dict[str, Any]] = guardian.get("branches", [])
    pos_or_branch = parsed.branch
    sigil = URI_INDEX_SIGIL_TOKEN
    positional = (
        pos_or_branch[len(sigil) :]
        if pos_or_branch.startswith(sigil)
        else (None if parsed.uri_form else pos_or_branch)
    )
    position: int | None = None
    if positional is not None:
        try:
            position = int(positional)
        except ValueError:
            if parsed.uri_form:
                raise SelectorError(
                    f"'?worktree={pos_or_branch}': '{sigil}' must be followed by a position"
                ) from None

    if position is None:
        # A worktree's label is its feature branch name -- what the Reviews UI
        # lists it under. Its stable `branch-...` id (RAL-122) is accepted too,
        # mirroring "the label, falling back to its id" at every other level of
        # the grammar (§C.2); an id match wins outright, since ids are unique
        # and a branch name need not be.
        by_id = [b for b in branches if str(b.get("id")) == pos_or_branch]
        matches = by_id or [b for b in branches if b.get("branch") == pos_or_branch]
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


# ── URI production (RAL-188 §C.3: ralphus always emits the ?id= sidecar) ──────


def run_view_uri(run: dict[str, Any], resolved: ResolvedSelector) -> str:
    """Render the canonical ralphus URI for whatever `resolved` addresses
    inside `run` (a `/api/runs/{id}` view).

    Every level falls back to its positional ``~N`` form when the entity has no
    name of its own, so an anonymous verify step is still addressable.
    """
    run_id = str(run.get("id") or resolved.run_id)
    label = str(run.get("label") or "") or run_id
    if resolved.kind == "run":
        return run_uri(run=label, run_id=run_id)

    tasks: list[dict[str, Any]] = run.get("tasks", [])
    task_view = tasks[resolved.task_idx] if resolved.task_idx < len(tasks) else {}
    task: str | int = str(task_view.get("name") or "") or resolved.task_idx

    session: str | int | None = None
    steps: list[dict[str, Any]] = []
    if resolved.verify_scope == "task":
        steps = task_view.get("verify", [])
    else:
        sessions: list[dict[str, Any]] = task_view.get("sessions", [])
        if 0 <= resolved.session_idx < len(sessions):
            session_view = sessions[resolved.session_idx]
            session = str(session_view.get("name") or session_view.get("id") or "")
            session = session or resolved.session_idx
            steps = session_view.get("verify", [])

    verify: str | int | None = None
    if resolved.kind == "verify":
        step = steps[resolved.verify_idx] if resolved.verify_idx < len(steps) else {}
        verify = str(step.get("id") or "") or resolved.verify_idx

    return run_uri(
        run=label,
        run_id=run_id,
        task=task,
        session=session,
        verify=verify,
        verify_scope=resolved.verify_scope,
    )


def guardian_view_uri(
    guardian: dict[str, Any], resolved: ResolvedGuardianSelector | None = None
) -> str:
    """Render the canonical ralphus URI for a review (a `/api/guardians/{id}`
    view), optionally narrowed to the branch/combined worktree `resolved`
    addresses."""
    guardian_id = str(guardian.get("id") or "")
    name = str(guardian.get("name") or "") or guardian_id
    if resolved is None:
        return review_uri(review=name, guardian_id=guardian_id)
    return review_uri(
        review=name,
        guardian_id=guardian_id or resolved.guardian_id,
        worktree=resolved.branch,
        combined=resolved.combined,
    )
