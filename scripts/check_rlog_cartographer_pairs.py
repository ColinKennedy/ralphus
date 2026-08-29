"""Lint daemon ``rlog!`` calls for a structured Cartographer counterpart.

The check is deliberately structural rather than adjacency-based: an ``rlog!``
passes when a nearby ``cartographer_log(...)`` or ``Note::...emit(...)`` call
shares a Rust brace-delimited logical block. This permits intervening statements
and paired match branches while keeping an event in another function from
satisfying the call.

Infrastructure diagnostics that cannot reach a ``Store`` may be exempted with
an immediately preceding or same-line comment of this form:

    // ralphus[ignore-rlog-pair]: substantive explanation of the exception

Stdlib-only so it can run directly from the repository root in CI.
"""

from __future__ import annotations

import re
import subprocess
import sys
from bisect import bisect_right
from dataclasses import dataclass
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parent.parent
DAEMON_SRC = REPO_ROOT / "daemon" / "src"
RLOG = re.compile(r"\b(?:crate\s*::\s*)?rlog\s*!")
STRUCTURED_LOG = re.compile(r"\b(?:cartographer_log|log_event(?:_with_task)?)\s*\(")
NOTE = re.compile(r"\bNote\s*::\s*new\s*\(")
EMIT = re.compile(r"\.\s*emit\s*\(")
EXEMPTION = re.compile(r"//\s*ralphus\[ignore-rlog-pair\]\s*:\s*(.*?)\s*$")
MAX_PAIR_LINES = 80


@dataclass(frozen=True)
class Block:
    start: int
    end: int


def tracked_daemon_sources() -> list[Path]:
    result = subprocess.run(
        ["git", "ls-files", "--", "daemon/src/*.rs"],
        cwd=REPO_ROOT,
        capture_output=True,
        text=True,
        check=True,
    )
    return [REPO_ROOT / line for line in result.stdout.splitlines() if line]


def mask_non_code(source: str) -> str:
    """Replace Rust comments and literals with spaces, preserving newlines."""
    out = list(source)
    i = 0
    block_depth = 0
    state = "code"
    raw_hashes = 0
    while i < len(source):
        if state == "line_comment":
            if source[i] == "\n":
                state = "code"
            else:
                out[i] = " "
            i += 1
        elif state == "block_comment":
            if source.startswith("/*", i):
                out[i : i + 2] = "  "
                block_depth += 1
                i += 2
            elif source.startswith("*/", i):
                out[i : i + 2] = "  "
                block_depth -= 1
                i += 2
                if block_depth == 0:
                    state = "code"
            else:
                if source[i] != "\n":
                    out[i] = " "
                i += 1
        elif state == "string":
            if source[i] == "\\":
                out[i] = " "
                if i + 1 < len(source):
                    if source[i + 1] != "\n":
                        out[i + 1] = " "
                    i += 2
                else:
                    i += 1
            else:
                char = source[i]
                if char != "\n":
                    out[i] = " "
                i += 1
                if char == '"':
                    state = "code"
        elif state == "raw_string":
            terminator = '"' + ("#" * raw_hashes)
            if source.startswith(terminator, i):
                out[i : i + len(terminator)] = " " * len(terminator)
                i += len(terminator)
                state = "code"
            else:
                if source[i] != "\n":
                    out[i] = " "
                i += 1
        elif source.startswith("//", i):
            out[i : i + 2] = "  "
            i += 2
            state = "line_comment"
        elif source.startswith("/*", i):
            out[i : i + 2] = "  "
            i += 2
            block_depth = 1
            state = "block_comment"
        elif source[i] == '"':
            out[i] = " "
            i += 1
            state = "string"
        elif source[i] == "'":
            char = re.match(r"'(?:\\.|[^\\'\n])'", source[i:])
            if char is None:
                # A Rust lifetime (`'a`) is code, not a character literal.
                i += 1
            else:
                token_len = len(char.group(0))
                out[i : i + token_len] = " " * token_len
                i += token_len
        elif source[i] == "r":
            raw = re.match(r'r(#+)?"', source[i:])
            if raw is None:
                i += 1
            else:
                token_len = len(raw.group(0))
                raw_hashes = len(raw.group(1) or "")
                out[i : i + token_len] = " " * token_len
                i += token_len
                state = "raw_string"
        else:
            i += 1
    return "".join(out)


def brace_blocks(code: str) -> list[Block]:
    stack: list[int] = []
    blocks: list[Block] = []
    for pos, char in enumerate(code):
        if char == "{":
            stack.append(pos)
        elif char == "}" and stack:
            blocks.append(Block(stack.pop(), pos + 1))
    blocks.append(Block(0, len(code)))
    return blocks


def cartographer_positions(code: str, blocks: list[Block]) -> list[int]:
    positions = [match.start() for match in STRUCTURED_LOG.finditer(code)]
    # `.emit` is only a Cartographer emitter in files using `Note::new`. Note
    # builders may be assigned, enriched, and emitted later, so do not require
    # the two tokens to be adjacent.
    if NOTE.search(code) is not None:
        positions.extend(match.start() for match in EMIT.finditer(code))
    # Follow thin, same-file wrappers around the primitive emitters. Store's
    # `log_event*` methods and runner lifecycle helpers intentionally centralize
    # Cartographer construction, and their call sites are valid pairings.
    functions: dict[str, Block] = {}
    for match in re.finditer(r"\bfn\s+([A-Za-z_][A-Za-z0-9_]*)\b", code):
        bodies = [block for block in blocks if block.start > match.end()]
        if bodies:
            functions[match.group(1)] = min(bodies, key=lambda block: block.start)
    helper_calls = {
        name: set(re.findall(r"\b([A-Za-z_][A-Za-z0-9_]*)\s*\(", code[body.start : body.end]))
        for name, body in functions.items()
    }
    helpers = {
        name
        for name, body in functions.items()
        if any(body.start <= pos < body.end for pos in positions)
    }
    changed = True
    while changed:
        changed = False
        for name, calls in helper_calls.items():
            if name not in helpers and not calls.isdisjoint(helpers):
                helpers.add(name)
                changed = True
    for helper in helpers:
        positions.extend(
            match.start()
            for match in re.finditer(rf"\b{re.escape(helper)}\s*\(", code)
        )
    return positions


def has_nearby_pair(
    call_line: int,
    call_blocks: frozenset[int],
    emits: list[tuple[int, frozenset[int]]],
) -> bool:
    for emit_line, emit_blocks in emits:
        if abs(call_line - emit_line) > MAX_PAIR_LINES:
            continue
        # Exclude the synthetic whole-file block: both sites must meet inside
        # an actual Rust block (normally a function, branch, or impl body).
        if not call_blocks.isdisjoint(emit_blocks):
            return True
    return False


def exemption_reason(lines: list[str], lineno: int) -> str | None:
    candidates = [lines[lineno - 1]]
    if lineno >= 2:
        candidates.append(lines[lineno - 2])
    for line in candidates:
        marker = EXEMPTION.search(line)
        if marker is not None:
            return marker.group(1)
    return None


def substantive(reason: str) -> bool:
    words = re.findall(r"[A-Za-z0-9]+", reason)
    return len(reason.strip()) >= 12 and len(words) >= 3


def source_violations(source: str) -> list[tuple[int, str]]:
    """Return ``(line, message)`` violations for one Rust source string."""
    violations: list[tuple[int, str]] = []
    code = mask_non_code(source)
    blocks = brace_blocks(code)
    emit_positions = cartographer_positions(code, blocks)
    line_starts = [match.end() for match in re.finditer("\n", source)]
    position_blocks = {
        pos: frozenset(
            index
            for index, block in enumerate(blocks)
            if block.start != 0 and block.start <= pos < block.end
        )
        for pos in emit_positions
    }
    emits = [
        (bisect_right(line_starts, pos), position_blocks[pos])
        for pos in emit_positions
    ]
    lines = source.splitlines()
    for call in RLOG.finditer(code):
        lineno = bisect_right(line_starts, call.start()) + 1
        reason = exemption_reason(lines, lineno)
        if reason is not None:
            if not substantive(reason):
                violations.append(
                    (
                        lineno,
                        "rlog exemption needs a substantive reason after "
                        "`ralphus[ignore-rlog-pair]:`",
                    )
                )
            continue
        call_blocks = frozenset(
            index
            for index, block in enumerate(blocks)
            if block.start != 0 and block.start <= call.start() < block.end
        )
        if not has_nearby_pair(
            bisect_right(line_starts, call.start()), call_blocks, emits
        ):
            violations.append(
                (
                    lineno,
                    "rlog! has no Cartographer emit in its logical block and no "
                    "`ralphus[ignore-rlog-pair]` justification",
                )
            )
    return violations


def find_violations() -> list[str]:
    violations: list[str] = []
    for path in tracked_daemon_sources():
        source = path.read_text(encoding="utf-8")
        rel = path.relative_to(REPO_ROOT).as_posix()
        violations.extend(
            f"{rel}:{lineno}: {message}"
            for lineno, message in source_violations(source)
        )
    return violations


def main() -> int:
    violations = find_violations()
    for violation in violations:
        print(violation)
    return 1 if violations else 0


if __name__ == "__main__":
    sys.exit(main())
