//! Optional tree-sitter parse of a Bash / PowerShell command line, for the
//! board's experimental agent log (RAL-644).
//!
//! The parse only changes how a command is *drawn*: it splits the text into
//! rows (one per top-level command) and tags byte ranges with a syntax role.
//! Nothing here executes, validates or rewrites a command, and no scheduling,
//! proof, review or run behaviour depends on it. Without the `parser` feature
//! [`parse`] compiles and answers [`Status::Unavailable`]. See
//! `docs/shell-syntax.md`.
//!
//! Rows: `;`, `&&`, `||`, `&` and newlines start a new row. A `|` pipeline is
//! one row (its stages are one logical step) with the `|` kept as an operator
//! span. A compound statement (`if`, `for`, a subshell) is one row.

use serde::{Deserialize, Serialize};

#[cfg(feature = "parser")]
mod engine;

/// Longest command text that is parsed, in bytes.
pub const MAX_INPUT_BYTES: usize = 16 * 1024;
/// Wall-clock budget for one parse, in milliseconds.
pub const PARSE_TIMEOUT_MS: u64 = 250;

/// Which grammar parses the text.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Shell {
    Bash,
    PowerShell,
}

/// The grammar for a recorded agent tool name, or `None` for a tool that is
/// not a shell. `exec` (Codex) does not say which shell it ran, so it follows
/// the host OS the daemon is running on (`host_is_windows`).
pub fn shell_for_tool(tool: &str, host_is_windows: bool) -> Option<Shell> {
    match tool {
        "Bash" | "bash" => Some(Shell::Bash),
        "PowerShell" | "powershell" => Some(Shell::PowerShell),
        "exec" => Some(if host_is_windows {
            Shell::PowerShell
        } else {
            Shell::Bash
        }),
        _ => None,
    }
}

/// Outcome of a parse request.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    /// Parsed cleanly; `commands` and `spans` are filled in.
    Ok,
    /// The grammar reported a syntax error; the board shows the raw text.
    Unparsed,
    /// The tool is not a shell tool.
    Unsupported,
    /// The text exceeds [`MAX_INPUT_BYTES`].
    TooLong,
    /// The parse ran past [`PARSE_TIMEOUT_MS`].
    Timeout,
    /// The feature is not built into this binary.
    Unavailable,
}

/// A syntax role a byte range can have.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SpanKind {
    String,
    Flag,
    Operator,
    CommandName,
    Variable,
    Comment,
    /// A heredoc / here-string body; its embedded language is not parsed.
    Heredoc,
}

/// One top-level command, in order.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Command {
    /// The command's text.
    pub text: String,
    /// Byte range of `text` in the input.
    pub start: usize,
    pub end: usize,
    /// The operator that joined this command to the previous one (`;`, `&&`,
    /// `||`, `&`), or "" for a newline and for the first command.
    pub sep: String,
}

/// A highlighted byte range of the input; ranges never overlap.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Span {
    pub kind: SpanKind,
    pub start: usize,
    pub end: usize,
}

/// The result of [`parse`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Parsed {
    pub status: Status,
    pub commands: Vec<Command>,
    pub spans: Vec<Span>,
}

impl Parsed {
    /// A result carrying only a status.
    pub fn empty(status: Status) -> Self {
        Self {
            status,
            commands: Vec::new(),
            spans: Vec::new(),
        }
    }
}

/// Whether this binary was built with the `parser` feature.
pub const fn available() -> bool {
    cfg!(feature = "parser")
}

/// Parses `text` with `shell`'s grammar. Never fails: every problem is a
/// [`Status`] with empty `commands` and `spans`.
pub fn parse(shell: Shell, text: &str) -> Parsed {
    if !available() {
        return Parsed::empty(Status::Unavailable);
    }
    if text.len() > MAX_INPUT_BYTES {
        return Parsed::empty(Status::TooLong);
    }
    #[cfg(feature = "parser")]
    {
        engine::parse(shell, text)
    }
    #[cfg(not(feature = "parser"))]
    {
        let _ = shell;
        Parsed::empty(Status::Unavailable)
    }
}

#[cfg(all(test, not(feature = "parser")))]
mod tests_off {
    use super::*;

    #[test]
    fn reports_unavailable_without_the_feature() {
        assert_eq!(parse(Shell::Bash, "ls").status, Status::Unavailable);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_tools_to_grammars() {
        assert_eq!(shell_for_tool("Bash", true), Some(Shell::Bash));
        assert_eq!(shell_for_tool("bash", false), Some(Shell::Bash));
        assert_eq!(shell_for_tool("PowerShell", false), Some(Shell::PowerShell));
        assert_eq!(shell_for_tool("powershell", false), Some(Shell::PowerShell));
        assert_eq!(shell_for_tool("exec", true), Some(Shell::PowerShell));
        assert_eq!(shell_for_tool("exec", false), Some(Shell::Bash));
        assert_eq!(shell_for_tool("Read", true), None);
    }
}
