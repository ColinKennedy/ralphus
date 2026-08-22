//! Shell selection + command-line construction for a user-supplied
//! program/script/compound-command (RAL-189), ported from
//! `cli/src/ralphus/shellcmd.py`. Used by [`crate::claude_code_backend`]/
//! [`crate::codex_backend`] to route a compound `RALPHUS_CLAUDE_COMMAND`/
//! `RALPHUS_CODEX_COMMAND` through the shell that launched `ralphus`, and
//! reused by the CLI's `quick-start --command` (via a path dependency on this
//! crate) for the same reason.
//!
//! Ancestry walking uses the `sysinfo` crate uniformly across platforms
//! (already a workspace dependency, used the same way by
//! `daemon/src/tmux.rs::find_server_pid_windows`) rather than Python's
//! separate Windows-`ctypes`/Linux-`/proc` implementations -- one
//! cross-platform code path, same behavior.

use std::path::Path;

use crate::hostos::is_windows;

pub const SHELL_AUTO: &str = "auto";

const REAL_SHELLS: &[&str] = &["powershell", "pwsh", "cmd", "bash", "sh", "zsh", "fish"];
const POWERSHELLS: &[&str] = &["powershell", "pwsh"];
const ANCESTRY_MAX_DEPTH: u32 = 12;

/// The argv prefix that makes each shell run one command line as a single
/// argument. PowerShell deliberately does NOT get `-NoProfile` -- the whole
/// point is that `--command` behaves like the same text typed at the user's
/// own prompt, profile included.
fn shell_prefix(shell: &str) -> &'static [&'static str] {
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

const WINDOWS_DIRECT_EXEC_SUFFIXES: &[&str] = &[".exe", ".com", ".bat", ".cmd"];
const DEFAULT_PATHEXT: &str = ".COM;.EXE;.BAT;.CMD";

fn shell_kind_from_exe(name: &str) -> Option<&'static str> {
    let cleaned = name.trim().trim_start_matches('-').to_lowercase();
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

/// Normalizes a `--shell`/`$RALPHUS_SHELL` value to a concrete shell name.
/// `None`/unrecognized both mean "detect the parent shell" -- there is no
/// failure mode here.
#[must_use]
pub fn resolve_shell(value: Option<&str>) -> String {
    if let Some(v) = value {
        let normalized = v.trim().to_lowercase();
        if REAL_SHELLS.contains(&normalized.as_str()) {
            return normalized;
        }
    }
    detect_parent_shell(&Env::from_process())
}

/// Environment inputs to shell detection, threaded explicitly (rather than
/// read from `std::env` inline) so tests can exercise every branch without
/// mutating real process environment -- `std::env::set_var`/`remove_var` are
/// `unsafe fn`s and this workspace forbids `unsafe_code` outright.
#[derive(Debug, Clone, Default)]
pub struct Env {
    pub ralphus_shell: Option<String>,
    pub powershell_distribution_channel: Option<String>,
    pub comspec: Option<String>,
    pub psmodulepath: Option<String>,
    pub shell: Option<String>,
}

impl Env {
    #[must_use]
    pub fn from_process() -> Self {
        Self {
            ralphus_shell: std::env::var("RALPHUS_SHELL").ok(),
            powershell_distribution_channel: std::env::var("POWERSHELL_DISTRIBUTION_CHANNEL").ok(),
            comspec: std::env::var("COMSPEC").ok(),
            psmodulepath: std::env::var("PSModulePath").ok(),
            shell: std::env::var("SHELL").ok(),
        }
    }
}

/// Best-effort: which shell launched this `ralphus` process. Tried in order:
/// `$RALPHUS_SHELL`; real process ancestry; environment heuristics; the
/// platform default.
#[must_use]
pub fn detect_parent_shell(env: &Env) -> String {
    if let Some(over) = env.ralphus_shell.as_deref().and_then(shell_kind_from_exe) {
        return over.to_string();
    }

    if let Some(from_ancestry) = ancestor_shell(ANCESTRY_MAX_DEPTH) {
        return from_ancestry;
    }

    if is_windows() {
        if env
            .powershell_distribution_channel
            .as_deref()
            .is_some_and(|v| !v.is_empty())
        {
            return "pwsh".to_string();
        }
        if let Some(kind) = env
            .comspec
            .as_deref()
            .and_then(|c| Path::new(c).file_name())
            .and_then(|n| n.to_str())
            .and_then(shell_kind_from_exe)
        {
            return kind.to_string();
        }
        if env.psmodulepath.as_deref().is_some_and(|v| !v.is_empty()) {
            return "powershell".to_string();
        }
        return "cmd".to_string();
    }

    env.shell
        .as_deref()
        .and_then(|s| Path::new(s).file_name())
        .and_then(|n| n.to_str())
        .and_then(shell_kind_from_exe)
        .unwrap_or("sh")
        .to_string()
}

/// The nearest shell among this process's ancestors, walked uniformly via
/// `sysinfo` (see module docs for why this is one code path, not two).
fn ancestor_shell(max_depth: u32) -> Option<String> {
    use sysinfo::{Pid, ProcessRefreshKind, ProcessesToUpdate, System};

    let mut sys = System::new();
    sys.refresh_processes_specifics(
        ProcessesToUpdate::All,
        true,
        ProcessRefreshKind::everything(),
    );

    let mut pid = Pid::from_u32(std::process::id());
    for _ in 0..max_depth {
        let parent = sys.process(pid)?.parent()?;
        let parent_proc = sys.process(parent)?;
        let name = parent_proc.name().to_string_lossy();
        if let Some(kind) = shell_kind_from_exe(&name) {
            return Some(kind.to_string());
        }
        if parent == pid {
            return None;
        }
        pid = parent;
    }
    None
}

/// Resolves `name` to a file the way the platform's own shells would: a
/// value containing a path separator is taken as a path and only checked for
/// existence; a bare name is searched in the current directory then `PATH` on
/// Windows (trying each `%PATHEXT%` suffix), `PATH` only on POSIX.
#[must_use]
pub fn find_program(name: &str) -> Option<String> {
    if name.is_empty() {
        return None;
    }
    if name.contains('/') || name.contains(std::path::MAIN_SEPARATOR) {
        let candidate = Path::new(name);
        return candidate
            .is_file()
            .then(|| candidate.to_string_lossy().into_owned());
    }

    let mut directories: Vec<String> = Vec::new();
    if is_windows() {
        if let Ok(cwd) = std::env::current_dir() {
            directories.push(cwd.to_string_lossy().into_owned());
        }
    }
    if let Ok(path) = std::env::var("PATH") {
        directories.extend(std::env::split_paths(&path).map(|p| p.to_string_lossy().into_owned()));
    }

    let mut suffixes = vec![String::new()];
    if is_windows() {
        let pathext = std::env::var("PATHEXT").unwrap_or_else(|_| DEFAULT_PATHEXT.to_string());
        suffixes.extend(
            pathext
                .split(';')
                .filter(|s| !s.is_empty())
                .map(str::to_string),
        );
    }

    for dir in &directories {
        for suffix in &suffixes {
            let candidate = Path::new(dir).join(format!("{name}{suffix}"));
            if candidate.is_file() {
                return Some(candidate.to_string_lossy().into_owned());
            }
        }
    }
    None
}

/// Can the OS `exec` `path` itself, without a shell interpreting it?
#[must_use]
pub fn is_directly_executable(path: &str) -> bool {
    if is_windows() {
        let ext = Path::new(path)
            .extension()
            .map(|e| format!(".{}", e.to_string_lossy().to_lowercase()));
        ext.is_some_and(|e| WINDOWS_DIRECT_EXEC_SUFFIXES.contains(&e.as_str()))
    } else {
        is_executable_posix(path)
    }
}

#[cfg(unix)]
fn is_executable_posix(path: &str) -> bool {
    use std::os::unix::fs::PermissionsExt as _;
    std::fs::metadata(path)
        .map(|m| m.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

#[cfg(not(unix))]
fn is_executable_posix(_path: &str) -> bool {
    false
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

/// Appends `args` (quoted for `shell`) to an opaque `raw_command` shell line.
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

/// The `(args, use_shell)` pair to hand a process spawn for `command_line`.
/// Everything gets an explicit argv except cmd.exe on Windows, which needs
/// `cmd.exe /C <string>` treated as one raw string -- cmd does not parse its
/// command line by MSVCRT argv-quoting rules, so requoting through an argv
/// vec would double-escape it.
pub enum SpawnArgs {
    Argv(Vec<String>),
    RawShellLine(String),
}

#[must_use]
pub fn shell_spawn_args(shell: &str, command_line: &str) -> SpawnArgs {
    if shell == "cmd" && is_windows() {
        return SpawnArgs::RawShellLine(command_line.to_string());
    }
    let prefix = shell_prefix(shell);
    let mut argv: Vec<String> = prefix.iter().map(|s| (*s).to_string()).collect();
    argv.push(command_line.to_string());
    SpawnArgs::Argv(argv)
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
    fn resolve_shell_prefers_explicit_value() {
        assert_eq!(resolve_shell(Some("bash")), "bash");
    }

    #[test]
    fn detect_parent_shell_uses_ralphus_shell_override() {
        let env = Env {
            ralphus_shell: Some("zsh".to_string()),
            ..Env::default()
        };
        assert_eq!(detect_parent_shell(&env), "zsh");
    }

    #[test]
    fn shell_kind_from_exe_strips_login_shell_dash() {
        assert_eq!(shell_kind_from_exe("-bash"), Some("bash"));
    }
}
