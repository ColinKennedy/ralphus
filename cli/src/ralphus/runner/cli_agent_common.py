"""Helpers shared by the external CLI-agent backends (``claude_code_backend.py``,
``codex_backend.py``): writing the prompt to a temp file (avoids OS command-line
length limits) and the live-session side-channel file used by the board's
"Watch Live"/"Open Agent" buttons.

Neither helper is specific to any one harness -- every CLI-driven backend
writes its prompt the same way and announces its session id the same way, so
this module exists to avoid re-implementing them per backend.
"""

from __future__ import annotations

import hashlib
import tempfile
from pathlib import Path

__all__ = ["RESUME_CONTINUATION_PROMPT", "live_session_path", "write_prompt_file"]

# Sent in place of the original prompt when resuming a session whose tmux pane
# vanished mid-run (see `daemon/src/runner.rs`'s `run_via_tmux` auto-reattach
# retry): the original conversation already has the real task instructions in
# its history, so resending the full prompt as a new turn would be redundant
# (and could confuse the model into thinking it's a distinct, second request)
# -- all that's needed is a nudge to keep going.
RESUME_CONTINUATION_PROMPT = (
    "The previous connection to this conversation was lost mid-task "
    "(not a decision by you or the user). Continue exactly where you "
    "left off and finish the task -- do not restart or repeat "
    "already-completed work."
)


def write_prompt_file(prompt: str) -> Path:
    """Write prompt to ~/.ralphus/task_prompts/<sha256>.md and return the path."""
    digest = hashlib.sha256(prompt.encode()).hexdigest()[:16]
    prompts_dir = Path.home() / ".ralphus" / "task_prompts"
    prompts_dir.mkdir(parents=True, exist_ok=True)
    path = prompts_dir / f"{digest}.md"
    path.write_text(prompt, encoding="utf-8")
    return path


def live_session_path(workspace_root: str | Path) -> Path:
    """Return the path where the session ID is written for Watch Live.

    Placed in the system temp dir so it is never tracked by git, never
    committed accidentally via ``git add -A``, and is accessible to both the
    Python runner and the Rust daemon on the same machine.

    The filename is derived from the worktree basename (e.g. ``wt-RAL-58-…``),
    which is globally unique per guardian-branch pair.
    """
    basename = Path(workspace_root).name or "unknown"
    live_dir = Path(tempfile.gettempdir()) / "ralphus"
    live_dir.mkdir(parents=True, exist_ok=True)
    return live_dir / f"{basename}.live_session"
