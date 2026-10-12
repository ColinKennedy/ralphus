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

/// The argv prefix that makes each known shell run one command line as a
/// single argument. PowerShell deliberately does NOT get `-NoProfile` -- the
/// whole point is that the command behaves like the same text typed at the
/// user's own prompt, profile included. An unrecognized `shell` falls back to
/// `sh -c`.
#[must_use]
pub fn shell_prefix(shell: &str) -> &'static [&'static str] {
    match shell {
        "powershell" => &["powershell", "-NoLogo", "-Command"],
        "pwsh" => &["pwsh", "-NoLogo", "-Command"],
        "cmd" => &["cmd.exe", "/C"],
        "bash" => &["bash", "-c"],
        "sh" => &["sh", "-c"],
        "zsh" => &["zsh", "-c"],
        "fish" => &["fish", "-c"],
        _ => &["sh", "-c"],
    }
}

/// The canonical shell name for an executable name or path (`pwsh.exe`,
/// `/usr/bin/bash`, a login shell's `-bash`), or `None`
/// when the basename is not a known shell.
#[must_use]
pub fn shell_kind_from_exe(name: &str) -> Option<&'static str> {
    let trimmed = name.trim();
    let base = trimmed.rsplit(['/', '\\']).next().unwrap_or(trimmed);
    let cleaned = base.trim_start_matches('-').to_lowercase();
    match cleaned.as_str() {
        "powershell" | "powershell.exe" => Some("powershell"),
        "pwsh" | "pwsh.exe" => Some("pwsh"),
        "cmd" | "cmd.exe" => Some("cmd"),
        "bash" | "bash.exe" => Some("bash"),
        "sh" | "sh.exe" | "dash" => Some("sh"),
        "zsh" | "zsh.exe" => Some("zsh"),
        "fish" | "fish.exe" => Some("fish"),
        _ => None,
    }
}

/// The flags that make the shell executable `shell` (a bare name or a path)
/// run one command line passed as the next argument. Chosen by the
/// executable's basename; an unrecognized shell gets `-c`.
#[must_use]
pub fn shell_command_flags(shell: &str) -> &'static [&'static str] {
    match shell_kind_from_exe(shell) {
        Some(kind) => &shell_prefix(kind)[1..],
        None => &["-c"],
    }
}

/// Build the [`std::process::Command`] that runs `command_line` through the
/// shell executable `shell`. The program is `shell` exactly as written (so a
/// path works and a bare name resolves on `PATH`). `cmd` on Windows takes the
/// line through `raw_arg` (cmd does not parse its command line by MSVCRT
/// argv-quoting rules, so `arg` would double-escape embedded quotes) with `/S`
/// so exactly the outer quote pair is stripped; every other shell receives
/// normal argv.
#[must_use]
pub fn shell_command(shell: &str, command_line: &str) -> std::process::Command {
    #[cfg(windows)]
    if shell_kind_from_exe(shell) == Some("cmd") {
        use std::os::windows::process::CommandExt as _;
        let mut cmd = std::process::Command::new(shell);
        cmd.raw_arg("/S")
            .raw_arg("/C")
            .raw_arg(format!("\"{command_line}\""));
        return cmd;
    }
    let mut cmd = std::process::Command::new(shell);
    cmd.args(shell_command_flags(shell)).arg(command_line);
    cmd
}

/// The message for a command whose selected shell could not be started.
#[must_use]
pub fn shell_spawn_error(shell: &str, error: &std::io::Error) -> String {
    if error.kind() == std::io::ErrorKind::NotFound {
        format!("shell `{shell}` was not found (is it installed and on PATH?): {error}")
    } else {
        format!("could not start shell `{shell}`: {error}")
    }
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

    #[test]
    fn shell_kind_from_exe_uses_the_basename() {
        assert_eq!(shell_kind_from_exe("/usr/bin/bash"), Some("bash"));
        assert_eq!(
            shell_kind_from_exe(r"C:\Program Files\PowerShell\7\pwsh.exe"),
            Some("pwsh")
        );
        assert_eq!(shell_kind_from_exe("-zsh"), Some("zsh"));
        assert_eq!(shell_kind_from_exe("nushell"), None);
    }

    #[test]
    fn shell_command_flags_follow_the_known_shell_table() {
        assert_eq!(shell_command_flags("pwsh"), ["-NoLogo", "-Command"]);
        assert_eq!(
            shell_command_flags("powershell.exe"),
            ["-NoLogo", "-Command"]
        );
        assert_eq!(shell_command_flags("cmd"), ["/C"]);
        assert_eq!(shell_command_flags("/bin/bash"), ["-c"]);
        assert_eq!(shell_command_flags("sh"), ["-c"]);
        assert_eq!(shell_command_flags("some-unknown-shell"), ["-c"]);
    }

    #[test]
    fn shell_command_passes_the_line_as_one_argument() {
        let cmd = shell_command("bash", "echo 'a b' ; echo c");
        let args: Vec<_> = cmd
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        assert_eq!(cmd.get_program(), "bash");
        assert_eq!(args, ["-c", "echo 'a b' ; echo c"]);
        let cmd = shell_command("pwsh", "a ; b");
        let args: Vec<_> = cmd
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        assert_eq!(args, ["-NoLogo", "-Command", "a ; b"]);
    }

    #[test]
    fn shell_spawn_error_names_the_shell() {
        let missing = std::io::Error::from(std::io::ErrorKind::NotFound);
        assert!(shell_spawn_error("fish", &missing).contains("`fish` was not found"));
        let denied = std::io::Error::from(std::io::ErrorKind::PermissionDenied);
        assert!(shell_spawn_error("fish", &denied).contains("could not start shell `fish`"));
    }
}
