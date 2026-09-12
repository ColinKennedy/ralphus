"""Check that every Guardian settings API field is TOML-declarable or explained.

The check reads the Rust source structurally so adding a field to
``GuardianSettingsBody`` cannot silently bypass the declarative ``[[review]]``
surface. It is stdlib-only for direct CI execution from the repository root.
"""

from __future__ import annotations

import re
import sys
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parent.parent
SERVER = REPO_ROOT / "daemon" / "src" / "server.rs"
VALIDATE = REPO_ROOT / "core" / "src" / "validate.rs"

# Settings-API key -> TOML key. Renames are documented here rather than hidden
# in the source parser.
MAPPING = {
    "skip_auto_build": "skip_auto_build",
    "skip_worktrees": "skip_worktrees",
    "resolver_agent": "agent",
    "resolver_model": "model",
    "proof_scope": "proof_scope",
    "proof_skip_auto_clean": "skip_auto_clean",
    "skip_base_updates": "skip_base_updates",
    "match_pr_branch_name": "match_pr_branch_name",
    "auto_submit_pr_stack": "auto_submit_pr_stack",
    "auto_pr_feedback": "auto_pr_feedback",
    "separate_pr_branch": "separate_pr_branch",
    "auto_fix_pr_errors": "auto_fix_pr_errors",
    "auto_fix_prompt_template": "auto_fix_prompt_template",
}
EXEMPT = {
    "base_branch": (
        "TOML surface is [[review]] upstream: guardian base_branch is minted "
        "from the declared or inferred upstream when the review is created."
    ),
}
IGNORE = re.compile(r"ralphus\[ignore-review-parity\]\s*:\s*(.*)$")
FIELD = re.compile(r"^\s*(\w+)\s*:\s*Option<")
KEY = re.compile(r'^\s*"([a-z_]+)",\s*$')


def substantive(reason: str) -> bool:
    """Return whether an exemption explains its current behavior."""
    return len(reason.strip()) >= 15 and len(re.findall(r"[A-Za-z0-9]+", reason)) >= 3


def brace_block(source: str, anchor: re.Pattern[str], label: str) -> str:
    """Extract the brace-delimited block beginning at ``anchor`` or raise."""
    match = anchor.search(source)
    if match is None:
        raise ValueError(f"anchor not found: {label}")
    start = source.find("{", match.start(), match.end() + 1)
    if start < 0:
        raise ValueError(f"anchor not found: opening brace for {label}")
    depth = 0
    for pos in range(start, len(source)):
        if source[pos] == "{":
            depth += 1
        elif source[pos] == "}":
            depth -= 1
            if depth == 0:
                return source[start + 1 : pos]
    raise ValueError(f"anchor not found: closing brace for {label}")


def settings_fields(source: str) -> dict[str, str | None]:
    """Return settings fields and any inline parity-exemption reason."""
    block = brace_block(source, re.compile(r"\bstruct\s+GuardianSettingsBody\s*\{"), "GuardianSettingsBody")
    fields: dict[str, str | None] = {}
    lines = block.splitlines()
    for index, line in enumerate(lines):
        match = FIELD.match(line)
        if match is None:
            continue
        comment = line[line.find("//") :] if "//" in line else ""
        if not comment and index > 0:
            comment = lines[index - 1]
        ignored = IGNORE.search(comment)
        fields[match.group(1)] = ignored.group(1) if ignored else None
    if "skip_auto_build" not in fields:
        raise ValueError("anchor not found: GuardianSettingsBody.skip_auto_build")
    return fields


def review_keys(source: str) -> set[str]:
    """Return the ``REVIEW_KEYS`` array or raise when its stable anchor moved."""
    anchor = re.compile(r"\bREVIEW_KEYS\s*:\s*&\[&str\]\s*=\s*&\[")
    match = anchor.search(source)
    if match is None:
        raise ValueError("anchor not found: REVIEW_KEYS")
    start = match.end() - 1
    depth = 0
    end = None
    for pos in range(start, len(source)):
        if source[pos] == "[":
            depth += 1
        elif source[pos] == "]":
            depth -= 1
            if depth == 0:
                end = pos
                break
    if end is None:
        raise ValueError("anchor not found: closing bracket for REVIEW_KEYS")
    block = source[start + 1 : end]
    keys = {match.group(1) for line in block.splitlines() if (match := KEY.match(line))}
    if "id" not in keys:
        raise ValueError("anchor not found: REVIEW_KEYS.id")
    return keys


def source_violations(server_source: str, validate_source: str) -> list[str]:
    """Return review-settings/TOML parity violations for supplied source text."""
    try:
        fields = settings_fields(server_source)
        keys = review_keys(validate_source)
    except ValueError as error:
        return [str(error)]
    violations: list[str] = []
    for field, inline_reason in fields.items():
        if inline_reason is not None:
            if not substantive(inline_reason):
                violations.append(f"{field}: inline review-parity exemption needs a substantive reason")
            continue
        toml_key = MAPPING.get(field)
        if toml_key is not None:
            if toml_key not in keys:
                violations.append(f"{field}: mapped [[review]] key {toml_key!r} is missing from REVIEW_KEYS")
            continue
        reason = EXEMPT.get(field)
        if reason is None:
            violations.append(
                f"{field}: settings key has no [[review]] mapping; add it to core/src/schema.rs "
                "+ REVIEW_KEYS, or exempt it here with a substantive reason"
            )
        elif not substantive(reason):
            violations.append(f"{field}: configured review-parity exemption needs a substantive reason")
    return violations


def main() -> int:
    violations = source_violations(SERVER.read_text(encoding="utf-8"), VALIDATE.read_text(encoding="utf-8"))
    for violation in violations:
        print(violation)
    return int(bool(violations))


if __name__ == "__main__":
    sys.exit(main())
