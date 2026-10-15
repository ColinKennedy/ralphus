# Shell syntax view (RAL-644, RAL-650)

The board's experimental Agent Log can draw a `Bash` / `PowerShell` / `cmd` tool call's
command as an ordered list of commands with highlighted strings, flags,
operators, variables and comments, instead of one flat run of text.

## Why it is optional

The parse uses tree-sitter and the Bash, PowerShell and cmd (`tree-sitter-batch`) grammars. The grammars
are C compiled with `cc`, so enabling the feature needs a working C compiler
and adds binary size and build time. It only changes how the log *looks*:
no scheduling, proof, review or run behaviour depends on it, and nothing here
executes, validates or rewrites a command.

## Building it

- Crate: `shell-syntax/` (`ralphus-shell-syntax`), feature `parser`.
- Daemon: `cargo build -p ralphus-daemon --features shell-syntax`.
- `scripts/build-debug.sh` turns it on; `scripts/build-release.sh` / `dist/` do
  not, until the PowerShell grammar has proven itself.
- CI tests both configurations: the default workspace run covers feature-off,
  and a separate step runs `-p ralphus-shell-syntax --features parser`.

Without the feature, `POST /api/shell-syntax/parse` answers
`{"status":"unavailable"}`, `GET /api/daemon` reports `shell_syntax: false`
(shown on the Health tab), and the board shows commands exactly as before.

## Behaviour

- Tool mapping: `Bash`/`bash` use the Bash grammar, `PowerShell`/`powershell`
  the PowerShell grammar, `cmd`/`cmd.exe` (any name `shellcmd::shell_kind_from_exe` maps to `cmd`) the cmd grammar, `exec` (Codex) follows the daemon host's OS
  (PowerShell on Windows, Bash elsewhere; a cell on a remote machine of another
  OS is still parsed with the daemon host's grammar). Any other tool is
  `unsupported`.
- Rows: `;` (Bash/PowerShell), `&&`, `||`, `&` and newlines start a new row. A `|` pipeline is
  one row with the `|` highlighted as an operator.
- cmd: the batch grammar needs a trailing newline, so the parser appends one (spans are unaffected). `^` escapes stay inside arguments; `%VAR%` and `%%i` are variables; `if`/`for` one-liners are one row, whatever the grammar produces.
- Heredoc / here-string bodies are one collapsed, unhighlighted block.
- Any grammar `ERROR` node makes the whole command `unparsed`; the board then
  shows the raw text. Input over 16 KiB (`too_long`) or a parse over 250 ms
  (`timeout`) likewise fall back to raw text.
- The daemon logs only the request's length and status, never the command text.

## On the board

A call is parsed once, when first expanded; collapsed calls and never-expanded
logs issue no requests, and results are cached per call. The expanded call has
a `raw`/`parsed` toggle so a mis-parse never hides what the agent ran. The
collapsed row shows `+N more` when the command has further rows (leading
`cd <dir>` rows are skipped, as in the preview). Colors are the `--sh-*`
variables in [`colors.md`](colors.md). After the daemon says `unavailable`, the
board stops asking for the rest of the session.
