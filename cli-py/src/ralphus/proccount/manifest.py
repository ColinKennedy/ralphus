"""The tagged-test manifest, ``proc_counts/tags.toml``.

Each ``[[test]]`` entry names one nextest test (``"<binary-id> <test name>"``)
and exactly one tag. A test appearing twice is an error: one tag each.
"""

from __future__ import annotations

import tomllib
from dataclasses import dataclass
from pathlib import Path

__all__ = ["TaggedTest", "load_manifest", "nextest_filter"]


@dataclass(frozen=True)
class TaggedTest:
    """One tagged test: ``binary_id`` + ``name`` form the nextest test id."""

    binary_id: str
    name: str
    tag: str

    @property
    def test_id(self) -> str:
        return f"{self.binary_id} {self.name}"


def load_manifest(path: Path) -> list[TaggedTest]:
    """Parse and validate the manifest; raises ``ValueError`` on a bad entry."""
    data = tomllib.loads(path.read_text(encoding="utf-8"))
    unknown = set(data) - {"test"}
    if unknown:
        raise ValueError(f"{path}: unknown top-level keys {sorted(unknown)}")
    tests: list[TaggedTest] = []
    seen: dict[str, str] = {}
    for index, entry in enumerate(data.get("test", []), start=1):
        test_id, tag = entry.get("id"), entry.get("tag")
        if not isinstance(test_id, str) or " " not in test_id.strip():
            raise ValueError(f"{path}: test #{index}: `id` must be '<binary-id> <test name>'")
        if not isinstance(tag, str) or not tag.strip():
            raise ValueError(f"{path}: test #{index}: `tag` must be a non-empty string")
        test_id = test_id.strip()
        if test_id in seen:
            raise ValueError(
                f"{path}: test {test_id!r} has more than one tag ({seen[test_id]!r}, {tag!r})"
            )
        seen[test_id] = tag
        binary_id, name = test_id.split(" ", 1)
        tests.append(TaggedTest(binary_id, name.strip(), tag.strip()))
    return tests


def nextest_filter(tests: list[TaggedTest]) -> str:
    """A nextest filterset expression selecting exactly ``tests``."""
    return " | ".join(f"(binary_id(={t.binary_id}) & test(={t.name}))" for t in tests)
