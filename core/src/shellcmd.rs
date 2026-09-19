//! Shell command-line quoting and compound-command detection, shared between
//! `ralphus-runner` (which routes a compound `RALPHUS_CLAUDE_COMMAND`/
//! `RALPHUS_CODEX_COMMAND`/`RALPHUS_PI_COMMAND` launcher through the shell
//! that started `ralphus` -- `runner/src/shellcmd.rs`,
//! `runner/src/cli_agent_common.rs`) and [`crate::agent_resume`] (which
//! resumes that same launcher into a live terminal, RAL-468).
//!
//! Both call sites need the identical "is this a user-authored shell line
//! rather than a single program name, and how do I quote one token for
//! `shell`" logic so a compound override (e.g. `rez-env foo -- claude`)
//! launches and resumes the same way, instead of each independently
//! mis-quoting the whole string as one literal program name.

/// PowerShell needs the `&` call operator to run a quoted string as a
/// command; both spellings resolve the same shell.
const POWERSHELLS: &[&str] = &["powershell", "pwsh"];

/// Whether `value` should be routed through a shell rather than treated as a
/// single program name: has a space, and isn't a single quoted/wrapped path
/// token (a fully quoted value -- e.g. `"C:\path with space\claude.exe"` --
/// is a single program path despite containing spaces, so it stays direct).
#[must_use]
pub fn is_compound_command(value: &str) -> bool {
    let trimmed = value.trim();
    if !trimmed.contains(' ') {
        return false;
    }
    !is_quote_wrapped(trimmed)
}

fn is_quote_wrapped(value: &str) -> bool {
    let bytes = value.as_bytes();
    bytes.len() >= 2
        && ((bytes[0] == b'"' && bytes[bytes.len() - 1] == b'"')
            || (bytes[0] == b'\'' && bytes[bytes.len() - 1] == b'\''))
}

/// Quotes `value` as exactly one literal token for `shell`.
#[must_use]
pub fn quote_for_shell(shell: &str, value: &str) -> String {
    if POWERSHELLS.contains(&shell) {
        return format!("'{}'", value.replace('\'', "''"));
    }
    if shell == "cmd" {
        if value.is_empty() || value.chars().any(|c| c == ' ' || c == '\t' || c == '"') {
            return format!("\"{}\"", value.replace('"', "\"\""));
        }
        return value.to_string();
    }
    if shell == "fish" {
        return format!("'{}'", value.replace('\\', "\\\\").replace('\'', "\\'"));
    }
    posix_quote(value)
}

/// Equivalent to Python's `shlex.quote`: wraps in single quotes unless the
/// value contains only shell-safe characters, splicing embedded single
/// quotes as `'\''`.
fn posix_quote(value: &str) -> String {
    let is_safe = !value.is_empty()
        && value
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "@%_+=:,./-".contains(c));
    if is_safe {
        return value.to_string();
    }
    format!("'{}'", value.replace('\'', "'\\''"))
}

/// A `shell` command line that runs `program` with `args`, all quoted.
/// PowerShell needs the `&` call operator -- a quoted string in command
/// position is just a string literal to it, not something to execute.
#[must_use]
pub fn build_program_command_line(shell: &str, program: &str, args: &[String]) -> String {
    let mut tokens = vec![quote_for_shell(shell, program)];
    tokens.extend(args.iter().map(|a| quote_for_shell(shell, a)));
    let line = tokens.join(" ");
    if POWERSHELLS.contains(&shell) {
        format!("& {line}")
    } else {
        line
    }
}

/// Appends `args` (quoted for `shell`) to an opaque `raw_command` shell line
/// -- used for a compound `program` (RAL-468: `rez-env foo -- claude`),
/// which is already a valid shell line and must run through the shell
/// verbatim rather than being quoted as a single literal program name.
#[must_use]
pub fn build_compound_command_line(shell: &str, raw_command: &str, args: &[String]) -> String {
    if args.is_empty() {
        return raw_command.to_string();
    }
    let tail = args
        .iter()
        .map(|a| quote_for_shell(shell, a))
        .collect::<Vec<_>>()
        .join(" ");
    format!("{raw_command} {tail}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quote_for_shell_powershell_doubles_single_quotes() {
        assert_eq!(quote_for_shell("powershell", "it's"), "'it''s'");
    }

    #[test]
    fn quote_for_shell_cmd_wraps_on_space() {
        assert_eq!(quote_for_shell("cmd", "hello world"), "\"hello world\"");
        assert_eq!(quote_for_shell("cmd", "noSpace"), "noSpace");
    }

    #[test]
    fn quote_for_shell_posix_passes_through_safe_tokens() {
        assert_eq!(quote_for_shell("bash", "safe-token_1"), "safe-token_1");
        assert_eq!(quote_for_shell("bash", "needs quoting"), "'needs quoting'");
    }

    #[test]
    fn build_program_command_line_adds_call_operator_for_powershell() {
        let line = build_program_command_line("powershell", "claude", &["--foo".to_string()]);
        assert!(line.starts_with("& "));
    }

    #[test]
    fn build_program_command_line_no_call_operator_for_bash() {
        let line = build_program_command_line("bash", "claude", &["--foo".to_string()]);
        assert!(!line.starts_with('&'));
    }

    #[test]
    fn build_compound_command_line_appends_quoted_args() {
        let line = build_compound_command_line("bash", "cd /foo && claude", &["a b".to_string()]);
        assert_eq!(line, "cd /foo && claude 'a b'");
    }

    #[test]
    fn is_compound_command_detects_multiword() {
        assert!(is_compound_command("cd /foo && claude"));
        assert!(!is_compound_command("claude"));
        assert!(!is_compound_command("\"C:\\path with space\\claude.exe\""));
    }

    #[test]
    fn is_compound_command_detects_double_dash_wrapper() {
        // RAL-468: a wrapper program (e.g. `rez-env`) that execs into the
        // real agent binary is a compound command, not a single launcher.
        assert!(is_compound_command("foo bar -- claude"));
    }
}
