# Agents

Agent profiles define how the daemon invokes backends (like Claude Code, Codex, or Pi), and their runtime configuration: command overrides, default models, and environment variables. The Agents tab is the admin-only interface for managing these profiles, persisted in the daemon's database rather than requiring hand-edited TOML files and a restart.

![The Agents tab showing a list of agent profiles, with built-in backends and custom profiles, and a form to create or edit profiles](../screenshots/agents-overview.png)

Each row represents one agent profile: its **name**, the **backend** it uses (built-in like `claude-code`, `codex`, `pi`, or custom backends like `ollama` or `raw`), an optional **default model**, the **command** if overridden from the backend's standard, and environment-variable **count**. Built-in backends are always present and non-deletable; their names (`claude-code`, `codex`, `pi`) appear in a fixed combo box when creating or editing a profile.

**Creating a profile** involves picking a backend from the combo box, optionally setting a default model, and maintaining a table of environment variables with columns for variable name, type (`Set` or `Link`), and value. `Set` stores a literal value (masked in the UI by default for security); `Link` references another environment variable in the same profile (resolving to a graph that is checked for cycles on save) or falls back to a value in the daemon's process environment.

**Editing a profile** stages changes locally — no live apply per keystroke. The "Save Agent" button validates the environment-variable graph for cycles first, blocking the save with a red error message naming the offending keys if a cycle is detected. Once saved, the profile takes effect on the next cell or proof run with no daemon restart required.

**Editing a built-in backend's command** (e.g., pointing `claude-code` at a wrapper script) changes behavior for every profile using that backend; the UI shows the list of affected profiles before you save, so the blast radius is clear.

**Deleting a profile** shows which stored cells, proofs, or reviews reference it and requires explicit confirmation — running invocations remain unchanged, but future starts or restarts use the current database state and may fail if a referenced profile no longer exists.

## MCP servers and skills (capabilities)

Below the profiles, the **Capabilities** section holds ralphus's own catalog of MCP servers and skills. A spawned agent gets *only* the items its profile has enabled, and nothing is enabled by default: personal MCP servers and skills from the user's own `~/.claude.json`, Codex or Pi configs are never picked up by an agent, even when personal settings are otherwise allowed.

**Catalog.** Each row shows the item's kind, name, source (`manual`, `import:<origin>` or `builtin`) and a status. An MCP server is `untested`, `ok` (with its tool count) or `failing` (with the reason); a skill is `found` or `missing`. **Check** probes a server on the daemon host (spawn, `initialize`, `tools/list`, stop) or looks for the skill folder's `SKILL.md`. **Import** reads the daemon user's `~/.claude.json`, `~/.codex/config.toml`, `~/.config/mcp/mcp.json` and skill folders, skipping names already present. MCP environment variables and HTTP headers use the same `Set`/`Link` model as profiles; `Set` values are masked and redacted everywhere.

**Per-profile toggles.** Selecting a profile — built-ins included — lists every catalog item with an enable checkbox. Claude Code receives the enabled servers through `--mcp-config` with `--strict-mcp-config` and its skills in the isolated config directory; Codex through `[mcp_servers.*]` in its isolated `CODEX_HOME`; Pi through an `mcp.json` there. The native agents (`claude`, `anthropic`, `ollama`) cannot receive either, and the tab says so. A profile on the `raw` backend has a **delivery** selector: the backend default, extra arguments appended to its command (`{mcp_config}` and `{skills_dir}` expand to the generated paths), or none.

**Per-tool allow/deny.** Each enabled MCP server expands into its tool list (filled by the server's last **Check**); unchecking a tool denies it for that profile. Claude Code enforces a deny with `--disallowedTools` and Codex with `disabled_tools`; Pi, the native agents and `extra_args` delivery cannot filter tools, and the tab says so instead of showing checkboxes. Tools are allowed unless denied, so a tool added by a server upgrade is allowed. A denied tool the server no longer lists stays visible as "(not listed)" until you clear it.

**The ralphus MCP server** is listed only when the global config sets `[agents] allow_ralphus_mcp = true`; it defaults to `false`.
