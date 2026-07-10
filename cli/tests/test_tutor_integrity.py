"""Validate that every TOML example embedded in TASK_TUTOR passes the schema.

Requires ralphus-daemon to be built (``cargo build -p ralphus-daemon``).
Skips gracefully if the binary is not found, matching the pattern used by the
Ollama integration tests.

External references (cross-run ``depends_on`` run-ids, worktree paths,
``<<upstream>>`` git sentinels) are all deferred to the daemon at run time and
are not resolved by the offline validator, so no mocking is needed.
"""

from __future__ import annotations

import os
import shutil
import subprocess
import sys
from pathlib import Path

import pytest

from ralphus.tutor import TASK_TUTOR


def _find_daemon_bin() -> str | None:
    """Locate the ralphus-daemon binary (mirrors __main__._find_daemon_bin)."""
    override = os.environ.get("RALPHUS_DAEMON_BIN")
    if override and (os.path.exists(override) or shutil.which(override)):
        return override
    # Prefer the Cargo workspace's debug build so tests always use the current
    # source rather than a potentially stale dist/ binary.
    here = Path(__file__).resolve()
    cargo_target = os.environ.get("CARGO_TARGET_DIR")
    for ancestor in [here, *here.parents]:
        if (ancestor / "Cargo.toml").exists():
            target_dirs = [ancestor / "target"]
            if cargo_target:
                target_dirs.insert(0, Path(cargo_target))
            for target_dir in target_dirs:
                for name in ("ralphus-daemon.exe", "ralphus-daemon"):
                    candidate = target_dir / "debug" / name
                    if candidate.exists():
                        return str(candidate)
            break
    search_dirs = [Path(sys.executable).resolve().parent, Path(sys.argv[0]).resolve().parent]
    for directory in search_dirs:
        for name in ("ralphus-daemon.exe", "ralphus-daemon"):
            candidate = directory / name
            if candidate.exists():
                return str(candidate)
    return shutil.which("ralphus-daemon")


def _examples_toml() -> str:
    """Extract the TOML examples block from TASK_TUTOR.

    The examples section runs from the first ``-- 1.`` separator to the
    ``Submit it:`` prose line that closes it.  Within that range the only
    non-TOML content is the ``-- N. Description ---`` separator lines
    themselves; stripping those yields a single valid TOML document
    containing all examples as one flat task array.
    """
    start = TASK_TUTOR.index("\n-- 1.")
    end = TASK_TUTOR.index("\nSubmit it:")
    section = TASK_TUTOR[start:end]
    lines = [line for line in section.splitlines() if not line.startswith("--")]
    return "\n".join(lines)


def test_tutor_examples_pass_schema_validation(tmp_path: Path) -> None:
    """All TOML examples in TASK_TUTOR must pass the ralphus-core schema validator."""
    daemon = _find_daemon_bin()
    if daemon is None:
        pytest.skip("ralphus-daemon not found — build it first: cargo build -p ralphus-daemon")

    toml_file = tmp_path / "tutor_examples.toml"
    toml_text = _examples_toml()
    toml_file.write_text(toml_text, encoding="utf-8")

    result = subprocess.run(
        [daemon, "validate", str(toml_file)],
        capture_output=True,
        text=True,
    )
    assert result.returncode == 0, (
        "One or more TOML examples in TASK_TUTOR failed schema validation.\n"
        f"stdout:\n{result.stdout}\n"
        f"stderr:\n{result.stderr}\n"
        "--- extracted TOML ---\n"
        f"{toml_text}"
    )
