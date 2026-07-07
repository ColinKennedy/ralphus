"""Tests for the agentic authoring orchestration and its CLI wiring.

The pydantic-ai generator is never touched here — a fake :class:`Generator`
returns canned TOML and a fake client stands in for the daemon, so the whole
generate → validate → submit → verify pipeline is exercised deterministically.
"""

from __future__ import annotations

import argparse
import sys
import tempfile
from pathlib import Path
from typing import Any

import pytest

import ralphus.__main__ as cli
from ralphus.author import agent as author_agent
from ralphus.author.core import (
    AuthorError,
    Budget,
    GenerateResult,
    GeneratorAborted,
    VerifyIntent,
    author_and_submit,
    build_system_prompt,
    build_user_prompt,
    parse_verify_answer,
)
from ralphus.client import DaemonError, ValidationOutcome

# ── Fakes ─────────────────────────────────────────────────────────────────────


class _FakeGenerator:
    """Returns queued results (or raises queued exceptions) per call."""

    def __init__(self, results: list[GenerateResult | Exception]) -> None:
        self._results = list(results)
        self.calls: list[tuple[str, str]] = []

    def generate(self, system_prompt: str, user_prompt: str, *, budget: Budget) -> GenerateResult:
        self.calls.append((system_prompt, user_prompt))
        item = self._results.pop(0)
        if isinstance(item, Exception):
            raise item
        return item


class _FakeClient:
    """A daemon stand-in: TOML containing 'BAD' is invalid; 'FAILSUBMIT' fails to submit."""

    def __init__(self, *, reviews: int = 1) -> None:
        self._reviews = reviews
        self.submitted: list[str] = []
        self._next = 0

    def validate(self, toml_text: str) -> ValidationOutcome:
        ok = "BAD" not in toml_text
        errors = [] if ok else [{"line": 1, "message": "bad token"}]
        return ValidationOutcome(valid=ok, errors=errors, warnings=[])

    def submit(
        self, toml_text: str, *, hold: bool = False, label: str | None = None
    ) -> dict[str, Any]:
        if "FAILSUBMIT" in toml_text:
            raise DaemonError("submit rejected")
        self.submitted.append(toml_text)
        self._next += 1
        return {"run_id": f"run-{self._next:012d}", "state": "queued" if hold else "pending"}

    def run(self, run_id: str) -> dict[str, Any]:
        return {"id": run_id, "reviews": [{"id": "g1"}] * self._reviews}


def _gen(*docs: str, tokens_in: int = 5, tokens_out: int = 5) -> _FakeGenerator:
    return _FakeGenerator(
        [GenerateResult(tomls=list(docs), tokens_in=tokens_in, tokens_out=tokens_out)]
    )


# ── parse_verify_answer ───────────────────────────────────────────────────────


@pytest.mark.parametrize(
    ("answer", "expected"),
    [
        ("", VerifyIntent()),
        ("no", VerifyIntent()),
        ("y", VerifyIntent(True, True, True)),
        ("all", VerifyIntent(True, True, True)),
        ("lint,test", VerifyIntent(False, True, True)),
        ("fmt lint", VerifyIntent(True, True, False)),
    ],
)
def test_parse_verify_answer(answer: str, expected: VerifyIntent) -> None:
    assert parse_verify_answer(answer) == expected


def test_parse_verify_answer_freeform_note() -> None:
    intent = parse_verify_answer("lint the api but skip tests elsewhere")
    assert not intent.formatting and not intent.testing
    # An unrecognized sentence is preserved verbatim as agent guidance.
    assert "lint the api" in intent.notes
    assert intent.any_requested


def test_parse_verify_answer_explain_uses_note() -> None:
    intent = parse_verify_answer("explain", note="test the db task only")
    assert intent.notes == "test the db task only"


# ── Prompt construction ───────────────────────────────────────────────────────


def test_build_system_prompt_review_toggles() -> None:
    with_review = build_system_prompt(VerifyIntent(), wants_review=True, tutor="TUTOR")
    without = build_system_prompt(VerifyIntent(), wants_review=False, tutor="TUTOR")
    assert "[[task.session.review]]" in with_review
    assert "MUST be reviewed" in with_review
    assert "Do NOT add any [[task.session.review]]" in without
    assert "TUTOR" in with_review  # schema reference is embedded


def test_build_system_prompt_verify_directive() -> None:
    prompt = build_system_prompt(
        VerifyIntent(True, True, True, notes="lint api, test the rest"),
        wants_review=False,
        tutor="",
    )
    assert "auto-formatter" in prompt
    assert "linter" in prompt
    assert "test suite" in prompt
    assert "lint api, test the rest" in prompt


def test_build_user_prompt_folds_feedback() -> None:
    assert "Produce the Task TOML now." in build_user_prompt("do x", None)
    retry = build_user_prompt("do x", "line 2: bad")
    assert "FAILED validation" in retry
    assert "line 2: bad" in retry


# ── Orchestration ─────────────────────────────────────────────────────────────


def test_happy_path_submits_and_counts_reviews() -> None:
    gen = _gen("good [[task]]")
    client = _FakeClient(reviews=2)
    outcome = author_and_submit(
        goal="build a thing",
        intent=VerifyIntent(),
        generator=gen,
        client=client,
        budget=Budget(),
        wants_review=True,
    )
    assert outcome.ok
    assert outcome.attempts == 1
    assert outcome.reviews_created == 2
    assert len(outcome.submitted) == 1
    assert client.submitted  # actually submitted


def test_validation_loop_retries_then_succeeds() -> None:
    gen = _FakeGenerator(
        [
            GenerateResult(tomls=["BAD first"], tokens_in=3, tokens_out=2),
            GenerateResult(tomls=["good second"], tokens_in=3, tokens_out=2),
        ]
    )
    outcome = author_and_submit(
        goal="x",
        intent=VerifyIntent(),
        generator=gen,
        client=_FakeClient(),
        budget=Budget(),
        max_attempts=5,
    )
    assert outcome.ok
    assert outcome.attempts == 2
    # The second call received the validation feedback.
    assert "FAILED validation" in gen.calls[1][1]


def test_never_valid_raises_after_max_attempts() -> None:
    gen = _FakeGenerator(
        [GenerateResult(tomls=["BAD"], tokens_in=1, tokens_out=1) for _ in range(3)]
    )
    with pytest.raises(AuthorError, match="within 3 attempt"):
        author_and_submit(
            goal="x",
            intent=VerifyIntent(),
            generator=gen,
            client=_FakeClient(),
            budget=Budget(),
            max_attempts=3,
        )
    assert len(gen.calls) == 3


def test_token_budget_aborts() -> None:
    gen = _gen("good", tokens_in=100, tokens_out=100)
    with pytest.raises(GeneratorAborted, match="token budget"):
        author_and_submit(
            goal="x",
            intent=VerifyIntent(),
            generator=gen,
            client=_FakeClient(),
            budget=Budget(max_tokens=50),
        )


def test_time_budget_aborts() -> None:
    clock = _Clock()

    class _SlowGenerator:
        def generate(self, _sys: str, _user: str, *, budget: Budget) -> GenerateResult:
            clock.advance(30.0)  # blow past the 10s deadline
            return GenerateResult(tomls=["good"], tokens_in=0, tokens_out=0)

    with pytest.raises(GeneratorAborted, match="time budget"):
        author_and_submit(
            goal="x",
            intent=VerifyIntent(),
            generator=_SlowGenerator(),
            client=_FakeClient(),
            budget=Budget(max_seconds=10.0, clock=clock),
        )


def test_partial_submission_is_reported() -> None:
    gen = _gen("good one\n---\nFAILSUBMIT two")
    outcome = author_and_submit(
        goal="x",
        intent=VerifyIntent(),
        generator=gen,
        client=_FakeClient(),
        budget=Budget(),
    )
    assert len(outcome.submitted) == 1
    assert len(outcome.failed) == 1
    assert "submit rejected" in outcome.failed[0].error
    assert not outcome.ok  # a failure means the run is not fully ok


def test_missing_required_review_is_not_ok() -> None:
    outcome = author_and_submit(
        goal="x",
        intent=VerifyIntent(),
        generator=_gen("good"),
        client=_FakeClient(reviews=0),
        budget=Budget(),
        wants_review=True,
    )
    assert outcome.reviews_created == 0
    assert not outcome.ok


def test_review_not_required_is_ok_without_reviews() -> None:
    outcome = author_and_submit(
        goal="x",
        intent=VerifyIntent(),
        generator=_gen("good"),
        client=_FakeClient(reviews=0),
        budget=Budget(),
        wants_review=False,
    )
    assert outcome.ok


def test_dry_run_writes_but_does_not_submit(tmp_path: Path) -> None:
    client = _FakeClient()
    outcome = author_and_submit(
        goal="x",
        intent=VerifyIntent(),
        generator=_gen("good one\n---\ngood two"),
        client=client,
        budget=Budget(),
        submit=False,
        workdir=tmp_path,
    )
    assert outcome.drafts == ["good one", "good two"]
    assert not outcome.submitted
    assert not client.submitted
    assert (tmp_path / "task-01.toml").exists()
    assert (tmp_path / "task-02.toml").exists()


def test_temp_dir_is_cleaned_up(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    made = tmp_path / "authtmp"
    made.mkdir()
    monkeypatch.setattr(tempfile, "mkdtemp", lambda prefix="": str(made))
    author_and_submit(
        goal="x",
        intent=VerifyIntent(),
        generator=_gen("good"),
        client=_FakeClient(),
        budget=Budget(),
    )
    assert not made.exists()  # owned temp dir removed on the way out


class _Clock:
    def __init__(self) -> None:
        self.t = 0.0

    def __call__(self) -> float:
        return self.t

    def advance(self, seconds: float) -> None:
        self.t += seconds


# ── Generator adapter (pure logic; no pydantic-ai needed) ─────────────────────


def test_build_model_rejects_unknown_backend() -> None:
    with pytest.raises(AuthorError, match="not supported"):
        author_agent._build_model("codex", None)


def test_as_author_error_maps_usage_limit_to_abort() -> None:
    class UsageLimitExceeded(Exception):
        pass

    aborted = author_agent._as_author_error(UsageLimitExceeded("over"))
    assert isinstance(aborted, GeneratorAborted)

    generic = author_agent._as_author_error(RuntimeError("boom"))
    assert isinstance(generic, AuthorError)
    assert not isinstance(generic, GeneratorAborted)


# ── CLI wiring ────────────────────────────────────────────────────────────────


@pytest.fixture
def _patch_cli(monkeypatch: pytest.MonkeyPatch) -> _FakeClient:
    client = _FakeClient()

    class _ClientCtx:
        def __init__(self, *_a: object, **_k: object) -> None:
            pass

        def __enter__(self) -> _FakeClient:
            return client

        def __exit__(self, *_exc: object) -> None:
            pass

    monkeypatch.setattr(cli, "DaemonClient", _ClientCtx)
    monkeypatch.setattr(cli, "_load_generator", lambda _a, _m: _gen("good [[task]]"))
    return client


def test_cli_author_submits(_patch_cli: _FakeClient, capsys: pytest.CaptureFixture[str]) -> None:
    code = cli.main(
        ["author", "--goal", "build x", "--verify", "all", "--review", "--label", "demo"]
    )
    assert code == 0
    out = capsys.readouterr().out
    assert "review(s) created" in out
    assert _patch_cli.submitted


def test_cli_author_dry_run(_patch_cli: _FakeClient, capsys: pytest.CaptureFixture[str]) -> None:
    code = cli.main(["author", "--goal", "build x", "--no-review", "--dry-run"])
    assert code == 0
    out = capsys.readouterr().out
    assert "draft 1" in out
    assert not _patch_cli.submitted


def test_cli_author_missing_extra(
    monkeypatch: pytest.MonkeyPatch, capsys: pytest.CaptureFixture[str]
) -> None:
    monkeypatch.setattr(cli, "_load_generator", lambda _a, _m: None)
    code = cli.main(["author", "--goal", "x", "--no-review", "--verify", "none"])
    assert code == 2
    assert "runner" in capsys.readouterr().err


def test_cli_author_prompt_file(
    _patch_cli: _FakeClient, monkeypatch: pytest.MonkeyPatch, tmp_path: Path
) -> None:
    ticket = tmp_path / "tickets.md"
    ticket.write_text("RAL-1: do the thing\nRAL-2: do the other thing\n", encoding="utf-8")
    gen = _gen("good [[task]]")
    monkeypatch.setattr(cli, "_load_generator", lambda _a, _m: gen)

    code = cli.main(["author", "--prompt-file", str(ticket), "--no-review", "--verify", "none"])
    assert code == 0
    assert _patch_cli.submitted
    # Whole-file contents flow through to the authoring prompt (the LLM splits
    # the tickets — no delimiter is parsed CLI-side).
    _system, user_prompt = gen.calls[0]
    assert "RAL-1: do the thing" in user_prompt
    assert "RAL-2: do the other thing" in user_prompt


def test_cli_author_prompt_file_missing(capsys: pytest.CaptureFixture[str], tmp_path: Path) -> None:
    missing = tmp_path / "nope.md"
    code = cli.main(["author", "--prompt-file", str(missing), "--no-review", "--verify", "none"])
    assert code == 2
    assert "prompt file not found" in capsys.readouterr().err


def test_cli_author_prompt_file_empty(capsys: pytest.CaptureFixture[str], tmp_path: Path) -> None:
    empty = tmp_path / "empty.md"
    empty.write_text("   \n", encoding="utf-8")
    code = cli.main(["author", "--prompt-file", str(empty), "--no-review", "--verify", "none"])
    assert code == 2
    assert "prompt file is empty" in capsys.readouterr().err


def test_resolve_goal_reads_prompt_file(tmp_path: Path) -> None:
    ticket = tmp_path / "t.md"
    ticket.write_text("ticket body", encoding="utf-8")
    args = argparse.Namespace(prompt_file=ticket, goal=None)
    assert cli._resolve_goal(args) == "ticket body"


def test_resolve_goal_inline_goal_preserved(tmp_path: Path) -> None:
    args = argparse.Namespace(prompt_file=None, goal="inline work")
    assert cli._resolve_goal(args) == "inline work"


def test_resolve_goal_interactive_path_is_read(
    monkeypatch: pytest.MonkeyPatch, tmp_path: Path
) -> None:
    ticket = tmp_path / "typed.md"
    ticket.write_text("typed ticket", encoding="utf-8")
    monkeypatch.setattr(sys.stdin, "isatty", lambda: True)
    # Emulate the user pasting a quoted path at the prompt.
    monkeypatch.setattr(cli, "_prompt", lambda _msg: f'"{ticket}"')
    args = argparse.Namespace(prompt_file=None, goal=None)
    assert cli._resolve_goal(args) == "typed ticket"


def test_resolve_goal_interactive_text_stays_inline(monkeypatch: pytest.MonkeyPatch) -> None:
    monkeypatch.setattr(sys.stdin, "isatty", lambda: True)
    monkeypatch.setattr(cli, "_prompt", lambda _msg: "just describe some work")
    args = argparse.Namespace(prompt_file=None, goal=None)
    assert cli._resolve_goal(args) == "just describe some work"


def test_load_generator_none_when_pydantic_ai_missing(monkeypatch: pytest.MonkeyPatch) -> None:
    # Regression: `_load_generator` used to only guard against `import
    # ralphus.author.agent` failing, but that module imports pydantic-ai
    # lazily inside `generate()`, so the import always succeeded and the
    # real ModuleNotFoundError crashed `author` later, unhandled.
    monkeypatch.setattr(cli, "pydantic_ai_available", lambda: False)
    assert cli._load_generator("claude", None) is None


def test_cli_author_review_required_but_missing(
    monkeypatch: pytest.MonkeyPatch, capsys: pytest.CaptureFixture[str]
) -> None:
    client = _FakeClient(reviews=0)

    class _ClientCtx:
        def __enter__(self) -> _FakeClient:
            return client

        def __exit__(self, *_exc: object) -> None:
            pass

    monkeypatch.setattr(cli, "DaemonClient", lambda *_a, **_k: _ClientCtx())
    monkeypatch.setattr(cli, "_load_generator", lambda _a, _m: _gen("good [[task]]"))
    code = cli.main(["author", "--goal", "x", "--review", "--verify", "none"])
    assert code == 1
    assert "a review was requested but none was created" in capsys.readouterr().err
