"""Agentic task authoring for the ``ralphus author`` command.

The pure orchestration lives in :mod:`ralphus.author.core`; the optional
pydantic-ai generator lives in :mod:`ralphus.author.agent` and is imported
lazily by the CLI.
"""

from __future__ import annotations

from ralphus.author.core import (
    AuthorError,
    AuthorOutcome,
    Budget,
    GenerateResult,
    Generator,
    GeneratorAborted,
    SubmitClient,
    SubmitFailure,
    SubmitRecord,
    VerifyIntent,
    author_and_submit,
    build_system_prompt,
    build_user_prompt,
    parse_verify_answer,
)

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
