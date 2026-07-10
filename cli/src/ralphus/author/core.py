"""Agentic task authoring: turn a plain-language goal into submitted tasks.

This module holds the *pure* orchestration for the ``ralphus author`` command —
the verify-intent model, the token/wall-clock budget, the generate→validate loop,
and the submit+verify step. It has no third-party dependencies and no interactive
IO, so it type-checks under mypy --strict and is exercised entirely with fakes.

The model-backed generator lives in ``ralphus.author.agent`` (the optional
``runner`` extra); it is injected here behind the :class:`Generator` protocol.
"""

from __future__ import annotations

import re
import shutil
import tempfile
import time
from collections.abc import Callable
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any, Protocol, runtime_checkable

from ralphus.client import DaemonError, ValidationOutcome
from ralphus.tutor import TASK_TUTOR

__all__ = [
    "AuthorError",
    "AuthorOutcome",
    "Budget",
    "GenerateResult",
    "Generator",
    "GeneratorAborted",
    "SubmitClient",
    "SubmitFailure",
    "SubmitRecord",
    "VerifyIntent",
    "author_and_submit",
    "build_system_prompt",
    "build_user_prompt",
    "parse_verify_answer",
]


class AuthorError(Exception):
    """Raised when authoring cannot produce a submittable result."""


class GeneratorAborted(AuthorError):
    """Raised when the authoring agent exceeds its token or time budget."""


# ── Verify intent ─────────────────────────────────────────────────────────────


@dataclass
class VerifyIntent:
    """The user's answer to 'attach fmt/lint/test verify steps?'.

    ``notes`` carries free-form per-task instructions ("lint the api task, test
    the rest") that the agent interprets; the booleans are the coarse toggles.
    """

    formatting: bool = False
    linting: bool = False
    testing: bool = False
    notes: str = ""

    @property
    def any_requested(self) -> bool:
        """True if any verify coverage was requested at all."""
        return self.formatting or self.linting or self.testing or bool(self.notes.strip())


_FMT = ("fmt", "format", "formatting", "formatter")
_LINT = ("lint", "linting", "linter")
_TEST = ("test", "tests", "testing")


def parse_verify_answer(answer: str, note: str = "") -> VerifyIntent:
    """Interpret a yes/no/explain verify answer into a :class:`VerifyIntent`.

    - empty / ``n`` / ``no`` / ``explain`` → nothing (plus any ``note``)
    - ``y`` / ``yes`` / ``all`` → all three checks
    - a bare keyword list like ``"lint,test"`` → just those checks
    - a free-form sentence → kept verbatim as ``notes`` (booleans left off) so the
      agent applies the per-task nuance precisely rather than a lossy toggle guess
    """
    text = answer.strip().lower()
    note = note.strip()
    if text in ("", "n", "no", "none", "explain", "e"):
        return VerifyIntent(notes=note)
    if text in ("y", "yes", "all"):
        return VerifyIntent(True, True, True, note)

    tokens = [t for t in re.split(r"[,\s]+", text) if t]
    check_words = set(_FMT) | set(_LINT) | set(_TEST)
    if any(t not in check_words for t in tokens):
        # A sentence, not a bare list: preserve it as authoritative guidance.
        return VerifyIntent(notes=f"{note} {answer.strip()}".strip())

    fmt = any(t in _FMT for t in tokens)
    lint = any(t in _LINT for t in tokens)
    test = any(t in _TEST for t in tokens)
    if not (fmt or lint or test):
        return VerifyIntent(notes=note)
    return VerifyIntent(fmt, lint, test, note)


# ── Budget (tokens + wall clock) ──────────────────────────────────────────────


@dataclass
class Budget:
    """A shared token + wall-clock budget for a whole authoring run.

    Call :meth:`start` once, :meth:`charge` after each agent call, and
    :meth:`check` at each loop boundary; a breach raises :class:`GeneratorAborted`.
    The generator also reads :meth:`remaining_tokens` / :meth:`remaining_seconds`
    to bound the model call itself.
    """

    max_tokens: int | None = None
    max_seconds: float | None = None
    clock: Callable[[], float] = time.monotonic
    _spent_tokens: int = 0
    _deadline: float | None = None

    def start(self) -> None:
        """Arm the wall-clock deadline (no-op if untimed)."""
        if self.max_seconds is not None:
            self._deadline = self.clock() + self.max_seconds

    def charge(self, tokens: int) -> None:
        """Record token spend from an agent call."""
        self._spent_tokens += max(0, tokens)

    @property
    def spent_tokens(self) -> int:
        """Tokens charged so far."""
        return self._spent_tokens

    def remaining_tokens(self) -> int | None:
        """Tokens left before the cap, or ``None`` when uncapped."""
        if self.max_tokens is None:
            return None
        return max(0, self.max_tokens - self._spent_tokens)

    def remaining_seconds(self) -> float | None:
        """Seconds left before the deadline, or ``None`` when untimed."""
        if self._deadline is None:
            return None
        return self._deadline - self.clock()

    def check(self) -> None:
        """Raise :class:`GeneratorAborted` if the token or time budget is spent."""
        if self.max_tokens is not None and self._spent_tokens >= self.max_tokens:
            raise GeneratorAborted(
                f"token budget exhausted ({self._spent_tokens}/{self.max_tokens} tokens)"
            )
        remaining = self.remaining_seconds()
        if remaining is not None and remaining <= 0:
            raise GeneratorAborted(f"time budget exhausted ({self.max_seconds:.0f}s)")


# ── Generator contract ────────────────────────────────────────────────────────


@dataclass
class GenerateResult:
    """One generation: the candidate TOML document(s) plus token usage."""

    tomls: list[str]
    tokens_in: int = 0
    tokens_out: int = 0


@runtime_checkable
class Generator(Protocol):
    """Produces Task TOML from a system + user prompt, honoring ``budget``."""

    def generate(self, system_prompt: str, user_prompt: str, *, budget: Budget) -> GenerateResult:
        """Return candidate TOML, aborting via :class:`GeneratorAborted` on breach."""
        ...


@runtime_checkable
class SubmitClient(Protocol):
    """The slice of :class:`ralphus.client.DaemonClient` authoring needs."""

    def validate(self, toml_text: str) -> ValidationOutcome:
        """Validate a TOML batch."""
        ...

    def submit(
        self, toml_text: str, *, hold: bool = False, label: str | None = None
    ) -> dict[str, Any]:
        """Submit a TOML batch, returning ``{run_id, state}``."""
        ...

    def run(self, run_id: str) -> dict[str, Any]:
        """Return a run's detail (used to count derived reviews)."""
        ...


# ── Result records ────────────────────────────────────────────────────────────


@dataclass
class SubmitRecord:
    """A successfully submitted document."""

    path: str
    run_id: str
    state: str
    reviews: int


@dataclass
class SubmitFailure:
    """A document that passed validation but failed to submit."""

    path: str
    error: str


@dataclass
class AuthorOutcome:
    """The overall result of an authoring run."""

    submitted: list[SubmitRecord] = field(default_factory=list)
    failed: list[SubmitFailure] = field(default_factory=list)
    drafts: list[str] = field(default_factory=list)
    attempts: int = 0
    review_required: bool = False

    @property
    def reviews_created(self) -> int:
        """Total reviews derived across every submitted run."""
        return sum(record.reviews for record in self.submitted)

    @property
    def ok(self) -> bool:
        """True when every document submitted and any required review exists."""
        if not self.submitted or self.failed:
            return False
        return not (self.review_required and self.reviews_created < 1)


# ── Prompt construction ───────────────────────────────────────────────────────


def build_system_prompt(
    intent: VerifyIntent, *, wants_review: bool, tutor: str = TASK_TUTOR
) -> str:
    """Assemble the authoring system prompt from the user's intent + the schema."""
    rules = [
        "You are a ralphus Task TOML author. Convert the user's plain-language "
        "request into one or more VALID ralphus Task TOML documents.",
        "",
        "Hard rules:",
        "- Output ONLY TOML. To emit multiple independent documents, separate each "
        "with a line containing exactly three dashes.",
        "- Every [[task.session]] needs a real absolute `cwd` (forward slashes on "
        "Windows) and exactly one of `prompt` or `command`.",
        "- Keep task and session names unique; wire ordering with `depends_on`.",
        _review_directive(wants_review),
        _verify_directive(intent),
        "",
        "The full schema reference and examples follow. Obey it exactly:",
        tutor,
    ]
    return "\n".join(line for line in rules if line is not None)


def _review_directive(wants_review: bool) -> str:
    if wants_review:
        return (
            "- This is a worktree feature that MUST be reviewed: add a top-level "
            '[[review]] block with id = "r", then set review = "r" on any '
            "[[task.session]] whose `cwd` is a git worktree with an upstream branch."
        )
    return "- Do NOT add any [[review]] blocks or review = ... fields."


def _verify_directive(intent: VerifyIntent) -> str:
    if not intent.any_requested:
        return "- Do NOT add [[task.session.verify]] steps unless the request itself demands one."
    wants: list[str] = []
    if intent.formatting:
        wants.append("an auto-formatter")
    if intent.linting:
        wants.append("a linter")
    if intent.testing:
        wants.append("the test suite")
    base = ""
    if wants:
        base = (
            "- Add [[task.session.verify]] `command` steps running "
            + ", ".join(wants)
            + " on the relevant sessions."
        )
    if intent.notes.strip():
        note = (
            "- Apply these per-task verify instructions precisely, adding only the "
            f"named checks to the named tasks: {intent.notes.strip()!r}."
        )
        return f"{base}\n{note}" if base else note
    return base


def build_user_prompt(goal: str, feedback: str | None) -> str:
    """Build the per-attempt user prompt, folding in validation ``feedback``."""
    goal = goal.strip()
    if feedback is None:
        return f"Work to accomplish:\n{goal}\n\nProduce the Task TOML now."
    return (
        f"Work to accomplish:\n{goal}\n\n"
        "Your previous TOML FAILED validation. Fix every error below and re-output "
        "the COMPLETE corrected TOML:\n"
        f"{feedback}\n\n"
        "Produce the corrected Task TOML now."
    )


# ── Document extraction ───────────────────────────────────────────────────────

_FENCE_RE = re.compile(r"(?s)```(?:toml)?\s*(.*?)```")
_SPLIT_RE = re.compile(r"(?m)^\s*-{3,}\s*$")


def _split_documents(text: str) -> list[str]:
    """Strip any markdown fences and split on lone ``---`` separators."""
    fences = _FENCE_RE.findall(text)
    body = "\n\n".join(fences) if fences else text
    docs = [part.strip() for part in _SPLIT_RE.split(body)]
    return [doc for doc in docs if doc]


def _format_errors(index: int, total: int, errors: list[dict[str, Any]]) -> str:
    where = f"document {index}/{total}" if total > 1 else "the document"
    lines = [f"  - line {err.get('line', '?')}: {err.get('message', 'invalid')}" for err in errors]
    return f"In {where}:\n" + "\n".join(lines)


# ── Orchestration ─────────────────────────────────────────────────────────────


def _noop(_message: str) -> None:
    return None


def _count_reviews(client: SubmitClient, run_id: str) -> int:
    if not run_id:
        return 0
    try:
        detail = client.run(run_id)
    except DaemonError:
        return 0
    reviews = detail.get("reviews")
    return len(reviews) if isinstance(reviews, list) else 0


def author_and_submit(
    *,
    goal: str,
    intent: VerifyIntent,
    generator: Generator,
    client: SubmitClient,
    budget: Budget,
    wants_review: bool = False,
    submit: bool = True,
    hold: bool = False,
    label: str | None = None,
    max_attempts: int = 5,
    tutor: str = TASK_TUTOR,
    workdir: Path | None = None,
    on_event: Callable[[str], None] = _noop,
) -> AuthorOutcome:
    """Drive goal → TOML → validate loop → submit, returning an :class:`AuthorOutcome`.

    ``generator`` produces candidate TOML; each attempt is validated through
    ``client`` and, on failure, the errors are fed back for a bounded number of
    ``max_attempts``. ``budget`` bounds tokens and wall-clock; a breach raises
    :class:`GeneratorAborted`. On success the documents are written to temp files
    (cleaned up unless ``workdir`` is given) and submitted unless ``submit`` is
    False (a dry run). ``wants_review`` both steers the prompt and gates success.
    """
    system_prompt = build_system_prompt(intent, wants_review=wants_review, tutor=tutor)
    budget.start()

    feedback: str | None = None
    valid_docs: list[str] = []
    attempts = 0
    for attempt in range(1, max_attempts + 1):
        attempts = attempt
        budget.check()
        on_event(f"generating TOML (attempt {attempt}/{max_attempts})")
        result = generator.generate(system_prompt, build_user_prompt(goal, feedback), budget=budget)
        budget.charge(result.tokens_in + result.tokens_out)
        budget.check()

        docs = [doc for raw in result.tomls for doc in _split_documents(raw)]
        if not docs:
            feedback = "No TOML was produced. Output at least one [[task]] document."
            on_event("agent produced no TOML; retrying")
            continue

        problems: list[str] = []
        for i, doc in enumerate(docs, 1):
            verdict = client.validate(doc)
            if not verdict.valid:
                problems.append(_format_errors(i, len(docs), verdict.errors))
        if not problems:
            valid_docs = docs
            break
        feedback = "\n".join(problems)
        on_event(f"validation failed for {len(problems)}/{len(docs)} document(s); retrying")
    else:
        raise AuthorError(
            f"agent did not produce valid TOML within {max_attempts} attempt(s). "
            f"Last validation feedback:\n{feedback}"
        )

    outcome = AuthorOutcome(
        drafts=list(valid_docs), attempts=attempts, review_required=wants_review
    )
    tmp = workdir if workdir is not None else Path(tempfile.mkdtemp(prefix="ralphus-author-"))
    owns_tmp = workdir is None
    try:
        paths: list[Path] = []
        for i, doc in enumerate(valid_docs, 1):
            path = tmp / f"task-{i:02d}.toml"
            path.write_text(doc, encoding="utf-8")
            paths.append(path)

        if not submit:
            on_event(f"dry run: wrote {len(paths)} document(s), nothing submitted")
            return outcome

        for path in paths:
            text = path.read_text(encoding="utf-8")
            on_event(f"submitting {path.name}")
            try:
                response = client.submit(text, hold=hold, label=label)
            except DaemonError as exc:
                outcome.failed.append(SubmitFailure(path=str(path), error=str(exc)))
                continue
            run_id = str(response.get("run_id", ""))
            outcome.submitted.append(
                SubmitRecord(
                    path=str(path),
                    run_id=run_id,
                    state=str(response.get("state", "")),
                    reviews=_count_reviews(client, run_id),
                )
            )
        return outcome
    finally:
        if owns_tmp:
            shutil.rmtree(tmp, ignore_errors=True)
