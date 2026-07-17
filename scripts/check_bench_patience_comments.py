"""Lint: explicit `ralphus_bench` patience values must carry a justification
comment (RAL-94/RAL-95).

Accepts either a trailing same-line comment or a comment on the line above.

Stdlib-only so it can run without `uv sync`. Invoke from the repo root:

    python scripts/check_bench_patience_comments.py
"""

from __future__ import annotations

import re
import subprocess
import sys
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parent.parent

# Tolerates cargo fmt / ruff format spacing variance around `=` and other
# attributes sharing the line, but stays anchored to the literal marker name
# so it can't drift onto unrelated calls.
PY_PATIENCE = re.compile(r"@pytest\.mark\.ralphus_bench\([^()]*\bpatience\s*=[^()]*\)")
RS_PATIENCE = re.compile(r"#\[\s*ralphus_bench\([^\[\]]*\bpatience\s*=[^\[\]]*\)\s*\]")

CHECKS = (
    ("*.py", PY_PATIENCE, "#"),
    ("*.rs", RS_PATIENCE, "//"),
)


def tracked_files(glob_pattern: str) -> list[Path]:
    result = subprocess.run(
        ["git", "ls-files", "--", glob_pattern],
        cwd=REPO_ROOT,
        capture_output=True,
        text=True,
        check=True,
    )
    return [REPO_ROOT / line for line in result.stdout.splitlines() if line]


def find_violations() -> list[str]:
    violations: list[str] = []
    for glob_pattern, pattern, comment_marker in CHECKS:
        for path in tracked_files(glob_pattern):
            text = path.read_text(encoding="utf-8")
            lines = text.splitlines()
            for lineno, line in enumerate(lines, start=1):
                match = pattern.search(line)
                if match is None:
                    continue
                if comment_marker in line[match.end() :]:
                    continue
                # A justification comment on the line immediately above is
                # also accepted, not just a same-line trailing comment.
                prev_line = lines[lineno - 2].strip() if lineno >= 2 else ""
                if prev_line.startswith(comment_marker):
                    continue
                rel = path.relative_to(REPO_ROOT).as_posix()
                violations.append(
                    f"{rel}:{lineno}: patience value set without an inline comment explaining why"
                )
    return violations


def main() -> int:
    violations = find_violations()
    for violation in violations:
        print(violation)
    return 1 if violations else 0


if __name__ == "__main__":
    sys.exit(main())
