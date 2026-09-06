//! Leaf commands that are genuinely not portable to a single MCP
//! request/response tool call, each with a required, non-empty reason
//! (RAL-301). This is a deliberate escape hatch, not a place to park
//! not-yet-implemented work -- `tests/parity.rs`'s
//! `every_exclusion_has_a_non_empty_reason` test enforces the "non-empty"
//! half locally and in CI; the "genuinely non-portable, not just
//! unimplemented" half is a human judgment call enforced by review.

/// `(leaf path, reason)`. Checked against `help_map::registered_leaves()` by
/// `tests/parity.rs`, so an exclusion for a path that doesn't exist (a typo,
/// or a path renamed out from under it) is caught the same way an
/// unimplemented tool is.
pub const EXCLUDED: &[(&[&str], &str)] = &[
    (
        &["cell", "open-agent"],
        "Opens the real interactive agent in a new terminal for a human to drive directly \
         (RAL-288) -- it hands control to a live, attached TTY session, not a value it could \
         return from one MCP tool call.",
    ),
    (
        &["cell", "remote-terminal"],
        "Attaches this process's own stdin/stdout to a live WebSocket byte relay onto a remote \
         cell's resumed Claude Code session (RAL-355 Phase 10) and blocks until the human ends \
         the session -- the remote counterpart of `cell open-agent`'s exclusion above, same \
         reason: a live, attached TTY session, not a value one MCP tool call could return.",
    ),
    (
        &["quick-start", "manager", "claude-code"],
        "Launches an interactive agent (Claude Code) preconfigured for the manager role and \
         drives it with an inherited TTY until the human ends the session -- the same reason \
         `help_map.rs` already excludes the whole `quick-start` group from its own AI-oriented \
         generated map.",
    ),
    (
        &["quick-start", "manager", "codex"],
        "See `quick-start manager claude-code`'s exclusion reason -- one interactive backend \
         choice for the same non-portable launch.",
    ),
    (
        &["quick-start", "manager", "pi"],
        "See `quick-start manager claude-code`'s exclusion reason -- one interactive backend \
         choice for the same non-portable launch.",
    ),
    (
        &["quick-start", "reviewer", "claude-code"],
        "Launches an interactive agent (Claude Code) preconfigured for the reviewer role and \
         drives it with an inherited TTY until the human ends the session -- see `quick-start \
         manager claude-code`'s exclusion reason.",
    ),
    (
        &["quick-start", "reviewer", "codex"],
        "See `quick-start reviewer claude-code`'s exclusion reason -- one interactive backend \
         choice for the same non-portable launch.",
    ),
    (
        &["quick-start", "reviewer", "pi"],
        "See `quick-start reviewer claude-code`'s exclusion reason -- one interactive backend \
         choice for the same non-portable launch.",
    ),
    (
        &["quick-start", "watcher", "claude-code"],
        "Launches an interactive agent (Claude Code) that monitors and drains the escalation \
         mailbox and drives it with an inherited TTY until the human ends the session -- see \
         `quick-start manager claude-code`'s exclusion reason.",
    ),
    (
        &["quick-start", "watcher", "codex"],
        "See `quick-start watcher claude-code`'s exclusion reason -- one interactive backend \
         choice for the same non-portable launch.",
    ),
    (
        &["quick-start", "watcher", "pi"],
        "See `quick-start watcher claude-code`'s exclusion reason -- one interactive backend \
         choice for the same non-portable launch.",
    ),
];

/// `true` if `path` is excluded from the MCP tool surface.
#[must_use]
pub fn is_excluded(path: &[&str]) -> bool {
    EXCLUDED.iter().any(|(p, _)| *p == path)
}
