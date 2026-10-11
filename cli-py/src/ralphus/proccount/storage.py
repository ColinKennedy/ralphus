"""``proc_counts/records.json``: a list of JSON blobs, one per git-tag label."""

from __future__ import annotations

import json
import re
from dataclasses import dataclass, field
from pathlib import Path

__all__ = ["CountRecord", "load_records", "save_records", "upsert_record"]


@dataclass
class CountRecord:
    """Process counts measured at one label (a git tag such as ``v0.0.2``)."""

    label: str
    commit: str
    platform: str
    counts: dict[str, int] = field(default_factory=dict)


def _label_key(label: str) -> tuple[int, tuple[int, ...]]:
    """Version-like labels sort numerically and first; others keep insertion order after."""
    match = re.fullmatch(r"v?(\d+(?:\.\d+)*)", label)
    if match is None:
        return (1, ())
    return (0, tuple(int(part) for part in match.group(1).split(".")))


def load_records(path: Path) -> list[CountRecord]:
    if not path.exists():
        return []
    return [
        CountRecord(
            label=str(blob["label"]),
            commit=str(blob["commit"]),
            platform=str(blob["platform"]),
            counts={str(k): int(v) for k, v in blob["counts"].items()},
        )
        for blob in json.loads(path.read_text(encoding="utf-8"))
    ]


def save_records(path: Path, records: list[CountRecord]) -> None:
    ordered = sorted(records, key=lambda r: _label_key(r.label))
    blobs = [
        {
            "label": r.label,
            "commit": r.commit,
            "platform": r.platform,
            "counts": dict(sorted(r.counts.items())),
        }
        for r in ordered
    ]
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(json.dumps(blobs, indent=2) + "\n", encoding="utf-8")


def upsert_record(records: list[CountRecord], new: CountRecord) -> list[CountRecord]:
    """Merge ``new`` into the record with the same label, replacing only the tests it measured.

    Every other test's entry is untouched, so adding a test never moves an existing line.
    """
    for existing in records:
        if existing.label == new.label:
            existing.commit = new.commit
            existing.platform = new.platform
            existing.counts.update(new.counts)
            return records
    records.append(new)
    return records
