# Agents

Agent profiles define how the daemon invokes backends (like Claude Code, Codex, or Pi), and their runtime configuration: command overrides, default models, and environment variables. The Agents tab is the admin-only interface for managing these profiles, persisted in the daemon's database rather than requiring hand-edited TOML files and a restart.

![The Agents tab showing a list of agent profiles, with built-in backends and custom profiles, and a form to create or edit profiles](../screenshots/agents-overview.png)

Each row represents one agent profile: its **name**, the **backend** it uses (built-in like `claude-code`, `codex`, `pi`, or custom backends like `ollama` or `raw`), an optional **default model**, the **command** if overridden from the backend's standard, and environment-variable **count**. Built-in backends are always present and non-deletable; their names (`claude-code`, `codex`, `pi`) appear in a fixed combo box when creating or editing a profile.

**Creating a profile** involves picking a backend from the combo box, optionally setting a default model, and maintaining a table of environment variables with columns for variable name, type (`Set` or `Link`), and value. `Set` stores a literal value (masked in the UI by default for security); `Link` references another environment variable in the same profile (resolving to a graph that is checked for cycles on save) or falls back to a value in the daemon's process environment.

**Editing a profile** stages changes locally — no live apply per keystroke. The "Save Agent" button validates the environment-variable graph for cycles first, blocking the save with a red error message naming the offending keys if a cycle is detected. Once saved, the profile takes effect on the next cell or proof run with no daemon restart required.

**Editing a built-in backend's command** (e.g., pointing `claude-code` at a wrapper script) changes behavior for every profile using that backend; the UI shows the list of affected profiles before you save, so the blast radius is clear.

**Deleting a profile** shows which stored cells, proofs, or reviews reference it and requires explicit confirmation — running invocations remain unchanged, but future starts or restarts use the current database state and may fail if a referenced profile no longer exists.
