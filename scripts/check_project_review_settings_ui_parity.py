"""Check that every [[review]] field with a project-level default has a UI/API equivalent.

RAL-408's settled decisions call for static enforcement that every
`[[review]]` TOML option the Arbiter's project-default fallback chain
already consults (`daemon::config::REVIEW_FIELD_PARITY`'s `ProjectDefault`
entries) is settable through the database-backed project-settings surface
this ticket adds -- `ProjectReviewSettingsBody` (daemon/src/server.rs) and
the board's Project Review Settings modal
(librarian/assets/board/76-project-review-settings-modal.js). Any field that
is deliberately NOT exposed there must be documented inline with a
substantive reason (see ``EXEMPT`` below), not silently skipped.

The check reads the Rust/JS source structurally rather than importing/
executing either, so it stays fast, dependency-free, and immune to either
side's own runtime state. It is stdlib-only for direct CI execution from the
repository root, mirroring ``check_review_settings_parity.py``'s shape.
"""

from __future__ import annotations

import re
import sys
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parent.parent
SERVER = REPO_ROOT / "daemon" / "src" / "server.rs"
CONFIG = REPO_ROOT / "daemon" / "src" / "config.rs"
MODAL = REPO_ROOT / "librarian" / "assets" / "board" / "76-project-review-settings-modal.js"

# `REVIEW_FIELD_PARITY` TOML key -> `ProjectReviewSettingsBody` field name.
# Renames are documented here rather than hidden in the source parser.
MAPPING = {
    "agent": "default_resolver_agent",
    "model": "default_resolver_model",
    "machine": "default_machine",
    "maximum_budget_usd": "default_maximum_budget_usd",
    "proof_scope": "default_proof_scope",
    "auto_submit_pr_stack": "auto_submit_pr_stack",
    "skip_worktrees": "skip_worktrees",
    "skip_base_updates": "skip_base_updates",
    "match_pr_branch_name": "match_pr_branch_name",
    "separate_pr_branch": "separate_pr_branch",
    "auto_build": "auto_build",
    "auto_fix_pr_errors": "auto_fix_pr_errors",
    "auto_fix_prompt_template": "auto_fix_prompt_template",
}
# `REVIEW_FIELD_PARITY` TOML key -> substantive reason it's deliberately not
# exposed by the project-settings API/UI despite having a `ProjectDefault`
# entry. Empty today -- every `ProjectDefault` key is covered via `MAPPING`.
EXEMPT: dict[str, str] = {}
FIELD = re.compile(r"^\s*(\w+)\s*:\s*Option<")
PROJECT_DEFAULT_KEY = re.compile(r'"([a-z_]+)",\s*\n\s*ReviewFieldDefault::ProjectDefault')


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


def settings_body_fields(source: str) -> set[str]:
    """Return `ProjectReviewSettingsBody`'s `Option<...>` field names."""
    block = brace_block(
        source,
        re.compile(r"\bstruct\s+ProjectReviewSettingsBody\s*\{"),
        "ProjectReviewSettingsBody",
    )
    fields = {m.group(1) for line in block.splitlines() if (m := FIELD.match(line))}
    if "default_resolver_agent" not in fields:
        raise ValueError("anchor not found: ProjectReviewSettingsBody.default_resolver_agent")
    return fields


def project_default_keys(source: str) -> set[str]:
    """Return every `[[review]]` key with a `ReviewFieldDefault::ProjectDefault` entry."""
    anchor = re.compile(r"\bREVIEW_FIELD_PARITY\s*:\s*&\[\(&str,\s*ReviewFieldDefault\)\]\s*=\s*&\[")
    match = anchor.search(source)
    if match is None:
        raise ValueError("anchor not found: REVIEW_FIELD_PARITY")
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
        raise ValueError("anchor not found: closing bracket for REVIEW_FIELD_PARITY")
    block = source[start + 1 : end]
    keys = {m.group(1) for m in PROJECT_DEFAULT_KEY.finditer(block)}
    if "agent" not in keys:
        raise ValueError("anchor not found: REVIEW_FIELD_PARITY ProjectDefault entry for \"agent\"")
    return keys


def source_violations(server_source: str, config_source: str, modal_source: str) -> list[str]:
    """Return TOML-project-default/UI-parity violations for supplied source text."""
    try:
        body_fields = settings_body_fields(server_source)
        keys = project_default_keys(config_source)
    except ValueError as error:
        return [str(error)]
    violations: list[str] = []
    for key in sorted(keys):
        reason = EXEMPT.get(key)
        if reason is not None:
            if not substantive(reason):
                violations.append(f"{key}: EXEMPT reason needs to be substantive")
            continue
        field = MAPPING.get(key)
        if field is None:
            violations.append(
                f"{key}: [[review]] field has a project default (REVIEW_FIELD_PARITY) but no "
                "entry in MAPPING; add one, or EXEMPT it here with a substantive reason"
            )
            continue
        if field not in body_fields:
            violations.append(
                f"{key}: mapped field {field!r} is missing from ProjectReviewSettingsBody "
                "(daemon/src/server.rs) -- add it, or EXEMPT this key with a substantive reason"
            )
            continue
        if field not in modal_source:
            violations.append(
                f"{key}: mapped field {field!r} has no visible reference in "
                f"{MODAL.relative_to(REPO_ROOT)} -- wire it into the board's Project Review "
                "Settings modal, or EXEMPT this key with a substantive reason"
            )
    return violations


def main() -> int:
    violations = source_violations(
        SERVER.read_text(encoding="utf-8"),
        CONFIG.read_text(encoding="utf-8"),
        MODAL.read_text(encoding="utf-8"),
    )
    for violation in violations:
        print(violation)
    return int(bool(violations))


if __name__ == "__main__":
    sys.exit(main())
