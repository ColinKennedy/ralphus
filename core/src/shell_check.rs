//! Offline check that a `command` is written for the shell it runs under
//! (RAL-651).
//!
//! Which shell a text belongs to is decided entirely by the tree-sitter
//! grammars in `ralphus-shell-syntax`: the command is parsed with every
//! grammar and the one reporting the fewest syntax errors fits best. Nothing
//! here inspects the text itself. Without the crate's `parser` feature every
//! check answers `None`, and a shell with no grammar is never checked.
//! Findings are advisory: the caller reports them as warnings.

use std::collections::HashSet;

use ralphus_shell_syntax::{Parsed, Shell, SpanKind, Status, parse, shell_for_tool};

use crate::shellcmd::shell_kind_from_exe;

const GRAMMARS: [Shell; 3] = [Shell::Bash, Shell::PowerShell, Shell::Cmd];

fn grammar_for(shell: &str) -> Option<Shell> {
    match shell_kind_from_exe(shell)? {
        "bash" | "sh" | "zsh" => Some(Shell::Bash),
        "powershell" | "pwsh" => Some(Shell::PowerShell),
        "cmd" => Some(Shell::Cmd),
        _ => None,
    }
}

fn name(shell: Shell) -> &'static str {
    match shell {
        Shell::Bash => "bash",
        Shell::PowerShell => "powershell",
        Shell::Cmd => "cmd",
    }
}

/// Syntax errors in a parse, or `None` when the grammar gave no verdict
/// (timeout, too long, parser not built in).
fn errors(parsed: &Parsed) -> Option<u32> {
    match parsed.status {
        Status::Ok => Some(0),
        Status::Unparsed => Some(parsed.error_count),
        _ => None,
    }
}

/// A warning message when `command` fits another shell's grammar better than
/// `shell` (the explicit `shell` of the cell or proof step, `None` when
/// unset), or `None` when it fits, ties, or cannot be checked. With no
/// explicit shell the host's default shell stands in for it.
#[must_use]
pub fn check_command(command: &str, shell: Option<&str>) -> Option<String> {
    let (declared, explicit) = match shell {
        Some(shell) => (grammar_for(shell)?, true),
        None => (shell_for_tool("exec", cfg!(windows))?, false),
    };
    let declared_parse = parse(declared, command);
    let declared_errors = errors(&declared_parse)?;
    if declared_errors == 0 {
        return None;
    }
    let best = GRAMMARS
        .into_iter()
        .filter(|g| *g != declared)
        .filter_map(|g| Some((errors(&parse(g, command))?, g)))
        .min_by_key(|(count, _)| *count);
    if let Some((count, other)) = best.filter(|(count, _)| *count < declared_errors) {
        let other_name = name(other);
        let verdict = if count == 0 {
            "is valid"
        } else {
            "fits better as"
        };
        return Some(if explicit {
            format!(
                "this command {verdict} {other_name} but its shell is {}; fix the command or set `shell = \"{other_name}\"`",
                name(declared),
            )
        } else {
            format!(
                "this command {verdict} {other_name}, not the default {}; want to set `shell = \"{other_name}\"` explicitly?",
                name(declared),
            )
        });
    }
    if !explicit {
        return None;
    }
    let at = declared_parse
        .error_line
        .map_or(String::new(), |n| format!(" at line {n} of the command"));
    Some(format!(
        "syntax error{at}: not valid {} syntax",
        name(declared)
    ))
}

/// Executable names of the commands in `command`, in order of appearance,
/// taken from the grammar's command-name nodes for the selected shell
/// (explicit `shell`, else the host default). Empty when the shell has no
/// grammar, the command does not parse cleanly, or the parser is not built in.
#[must_use]
pub fn executable_names(command: &str, shell: Option<&str>) -> Vec<String> {
    let Some((grammar, _)) = selected_grammar(shell) else {
        return Vec::new();
    };
    let parsed = parse(grammar, command);
    if parsed.status != Status::Ok {
        return Vec::new();
    }
    parsed
        .spans
        .iter()
        .filter(|s| s.kind == SpanKind::CommandName)
        .filter_map(|s| command.get(s.start..s.end))
        .map(|n| n.trim().to_string())
        .filter(|n| !n.is_empty())
        .collect()
}

fn selected_grammar(shell: Option<&str>) -> Option<(Shell, &'static str)> {
    let grammar = match shell {
        Some(shell) => grammar_for(shell)?,
        None => shell_for_tool("exec", cfg!(windows))?,
    };
    Some((grammar, name(grammar)))
}

/// Warnings for executables in `command` that cannot be found on this host.
/// `seen` holds the `shell:executable` keys already looked up during this
/// validation: an executable already in it is neither looked up nor reported
/// again, and new ones are added. Names that cannot be judged offline (shell
/// built-ins, cmdlets, aliases, variables, paths) are skipped.
#[must_use]
pub fn missing_command_warnings(
    command: &str,
    shell: Option<&str>,
    seen: &mut HashSet<String>,
) -> Vec<String> {
    missing_command_warnings_with(command, shell, seen, &|exe| find_executable(exe, None))
}

fn missing_command_warnings_with(
    command: &str,
    shell: Option<&str>,
    seen: &mut HashSet<String>,
    exists: &dyn Fn(&str) -> bool,
) -> Vec<String> {
    let Some((grammar, shell_name)) = selected_grammar(shell) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for exe in executable_names(command, shell) {
        if !seen.insert(format!("{shell_name}:{}", exe.to_lowercase())) {
            continue;
        }
        if !is_checkable(grammar, &exe) || exists(&exe) {
            continue;
        }
        out.push(format!(
            "`{exe}` was not found on this machine's PATH for {shell_name}; check the selected shell or correct the executable name (aliases, shell functions and tools installed only in a later execution environment cannot be seen here)"
        ));
    }
    out
}

/// Whether `exe` is a plain program name whose presence on `PATH` says
/// something: not a built-in, cmdlet, alias, path, variable or glob.
fn is_checkable(grammar: Shell, exe: &str) -> bool {
    if exe.contains([
        '/', '\\', '$', '%', '`', '*', '?', '(', ')', '{', '}', '"', '\'', '~', '=',
    ]) {
        return false;
    }
    let lower = exe.to_lowercase();
    match grammar {
        Shell::Bash => !BASH_BUILTINS.contains(&lower.as_str()),
        Shell::Cmd => !CMD_BUILTINS.contains(&lower.as_str()),
        Shell::PowerShell => !PS_ALIASES.contains(&lower.as_str()) && !is_cmdlet_name(&lower),
    }
}

fn is_cmdlet_name(lower: &str) -> bool {
    lower
        .split_once('-')
        .is_some_and(|(verb, _)| PS_VERBS.contains(&verb))
}

const BASH_BUILTINS: &[&str] = &[
    ".",
    ":",
    "[",
    "[[",
    "alias",
    "bg",
    "bind",
    "break",
    "builtin",
    "caller",
    "cd",
    "command",
    "compgen",
    "complete",
    "continue",
    "declare",
    "dirs",
    "disown",
    "echo",
    "enable",
    "eval",
    "exec",
    "exit",
    "export",
    "false",
    "fc",
    "fg",
    "getopts",
    "hash",
    "help",
    "history",
    "jobs",
    "kill",
    "let",
    "local",
    "logout",
    "mapfile",
    "popd",
    "printf",
    "pushd",
    "pwd",
    "read",
    "readarray",
    "readonly",
    "return",
    "set",
    "shift",
    "shopt",
    "source",
    "suspend",
    "test",
    "times",
    "trap",
    "true",
    "type",
    "typeset",
    "ulimit",
    "umask",
    "unalias",
    "unset",
    "wait",
];

const CMD_BUILTINS: &[&str] = &[
    "assoc", "break", "call", "cd", "chdir", "cls", "color", "copy", "date", "del", "dir", "echo",
    "endlocal", "erase", "exit", "for", "ftype", "goto", "if", "md", "mkdir", "mklink", "move",
    "path", "pause", "popd", "prompt", "pushd", "rd", "rem", "ren", "rename", "rmdir", "set",
    "setlocal", "shift", "start", "time", "title", "type", "ver", "verify", "vol",
];

const PS_ALIASES: &[&str] = &[
    "%", "?", "cat", "cd", "chdir", "clear", "cls", "copy", "cp", "curl", "del", "dir", "echo",
    "erase", "fl", "foreach", "ft", "gc", "gci", "gi", "gl", "gm", "gp", "gps", "gv", "h",
    "history", "icm", "iex", "irm", "iwr", "kill", "ls", "man", "md", "measure", "mi", "mkdir",
    "move", "mv", "ni", "oss", "popd", "ps", "pushd", "pwd", "rd", "ren", "ri", "rm", "rmdir",
    "rni", "sc", "select", "set", "si", "sl", "sleep", "sls", "sort", "start", "tee", "type",
    "where", "wget", "write",
];

const PS_VERBS: &[&str] = &[
    "add",
    "clear",
    "compare",
    "compress",
    "convert",
    "convertfrom",
    "convertto",
    "copy",
    "disable",
    "enable",
    "enter",
    "exit",
    "expand",
    "export",
    "find",
    "foreach",
    "format",
    "get",
    "group",
    "hide",
    "import",
    "install",
    "invoke",
    "join",
    "lock",
    "measure",
    "move",
    "new",
    "out",
    "pop",
    "publish",
    "push",
    "read",
    "receive",
    "register",
    "remove",
    "rename",
    "resolve",
    "restart",
    "resume",
    "save",
    "select",
    "send",
    "set",
    "show",
    "sort",
    "split",
    "start",
    "stop",
    "suspend",
    "test",
    "uninstall",
    "unlock",
    "unregister",
    "update",
    "use",
    "wait",
    "where",
    "write",
];

/// Looks `exe` up on `path` (the process `PATH` when `None`), trying the
/// `PATHEXT` extensions on Windows.
fn find_executable(exe: &str, path: Option<&std::ffi::OsStr>) -> bool {
    let path = path
        .map(std::ffi::OsStr::to_os_string)
        .or_else(|| std::env::var_os("PATH"));
    let Some(path) = path else {
        return false;
    };
    let exts: Vec<String> = if cfg!(windows) {
        let pathext =
            std::env::var("PATHEXT").unwrap_or_else(|_| ".COM;.EXE;.BAT;.CMD".to_string());
        std::iter::once(String::new())
            .chain(
                pathext
                    .split(';')
                    .filter(|e| !e.is_empty())
                    .map(str::to_string),
            )
            .chain(std::iter::once(".ps1".to_string()))
            .collect()
    } else {
        vec![String::new()]
    };
    std::env::split_paths(&path).any(|dir| {
        exts.iter().any(|ext| {
            let candidate = dir.join(format!("{exe}{ext}"));
            candidate.is_file() && is_runnable(&candidate)
        })
    })
}

#[cfg(unix)]
fn is_runnable(path: &std::path::Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path).is_ok_and(|m| m.permissions().mode() & 0o111 != 0)
}

#[cfg(not(unix))]
fn is_runnable(_path: &std::path::Path) -> bool {
    true
}

#[cfg(all(test, feature = "shell-syntax"))]
mod tests {
    use super::*;

    fn warnings(command: &str, shell: Option<&str>, present: &[&str]) -> Vec<String> {
        let mut seen = HashSet::new();
        missing_command_warnings_with(command, shell, &mut seen, &|e| present.contains(&e))
    }

    #[test]
    fn extracts_executables_across_chains_and_pipes() {
        assert_eq!(
            executable_names("cargo build && npm test | tee out; ls", Some("bash")),
            ["cargo", "npm", "tee", "ls"]
        );
    }

    #[test]
    fn extracts_powershell_and_cmd_executables() {
        assert_eq!(
            executable_names(
                "git status; Get-ChildItem | Select-Object Name",
                Some("powershell")
            ),
            ["git", "Get-ChildItem", "Select-Object"]
        );
        assert_eq!(executable_names("cargo test", Some("cmd")), ["cargo"]);
    }

    #[test]
    fn missing_executable_warns_with_guidance() {
        let w = warnings("nosuchtool --flag", Some("bash"), &[]);
        assert_eq!(w.len(), 1);
        assert!(
            w[0].contains("nosuchtool") && w[0].contains("selected shell"),
            "{}",
            w[0]
        );
        assert!(w[0].contains("correct the executable name"), "{}", w[0]);
    }

    #[test]
    fn present_executable_is_silent() {
        assert!(warnings("cargo test", Some("bash"), &["cargo"]).is_empty());
    }

    #[test]
    fn repeated_executables_are_checked_once() {
        let calls = std::cell::Cell::new(0);
        let mut seen = HashSet::new();
        let exists = |_: &str| {
            calls.set(calls.get() + 1);
            false
        };
        let first = missing_command_warnings_with(
            "zzz a && zzz b; zzz c",
            Some("bash"),
            &mut seen,
            &exists,
        );
        let second = missing_command_warnings_with("zzz d", Some("bash"), &mut seen, &exists);
        assert_eq!(first.len(), 1);
        assert!(second.is_empty());
        assert_eq!(calls.get(), 1);
    }

    #[test]
    fn shell_builtins_and_cmdlets_are_not_looked_up() {
        assert!(warnings("cd /tmp && echo hi", Some("bash"), &[]).is_empty());
        assert!(warnings("Get-ChildItem | ls", Some("powershell"), &[]).is_empty());
        assert!(warnings("dir && echo hi", Some("cmd"), &[]).is_empty());
    }

    #[test]
    fn builtin_set_depends_on_the_shell() {
        // `dir` is a cmd/PowerShell built-in but an external program in bash.
        assert_eq!(warnings("dir", Some("bash"), &[]).len(), 1);
        assert!(warnings("dir", Some("cmd"), &[]).is_empty());
        // `export` is a bash built-in only.
        assert!(warnings("export A=1", Some("bash"), &[]).is_empty());
    }

    #[test]
    fn unparsable_or_unknown_shell_is_not_checked() {
        assert!(warnings("if then ((", Some("bash"), &[]).is_empty());
        assert!(warnings("nosuchtool", Some("fish"), &[]).is_empty());
    }

    #[test]
    fn path_lookup_finds_files_in_listed_directories() {
        let dir = std::env::temp_dir().join(format!("ralphus-shcheck-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join(if cfg!(windows) {
            "mytool.exe"
        } else {
            "mytool"
        });
        std::fs::write(&file, b"").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let path = std::env::join_paths([&dir]).unwrap();
        assert!(find_executable("mytool", Some(&path)));
        assert!(!find_executable("othertool", Some(&path)));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn ambiguous_command_is_silent() {
        for shell in [None, Some("bash"), Some("powershell")] {
            assert_eq!(check_command("cargo test", shell), None);
        }
    }

    #[test]
    fn unset_shell_compares_against_the_host_default() {
        let msg = check_command("if [ -f a ]; then echo hi; fi", None);
        if cfg!(windows) {
            assert!(msg.unwrap().contains("shell = \"bash\""));
        } else {
            assert_eq!(msg, None);
        }
        let msg = check_command("foreach ($i in 1..3) { Write-Host $i }", None);
        if cfg!(windows) {
            assert_eq!(msg, None);
        } else {
            assert!(msg.unwrap().contains("shell = \"powershell\""));
        }
    }

    #[test]
    fn bash_command_under_powershell_names_both_shells() {
        let msg = check_command("if [ -f a ]; then echo hi; fi", Some("powershell")).unwrap();
        assert!(msg.contains("bash") && msg.contains("powershell"), "{msg}");
    }

    #[test]
    fn powershell_command_under_bash_names_both_shells() {
        let msg = check_command("foreach ($i in 1..3) { Write-Host $i }", Some("bash")).unwrap();
        assert!(msg.contains("shell = \"powershell\""), "{msg}");
    }

    #[test]
    fn syntax_error_reports_line() {
        let msg = check_command("echo ok\nif then ((", Some("bash")).unwrap();
        assert!(msg.contains("line 2"), "{msg}");
    }

    #[test]
    fn unknown_shell_is_not_checked() {
        assert_eq!(check_command("if then ((", Some("fish")), None);
    }
}

#[cfg(test)]
mod tests_off {
    use super::*;

    #[test]
    fn skipped_without_the_feature() {
        // Cargo feature unification can enable the parser through the
        // dependency alone, without this crate's `shell-syntax` feature.
        if ralphus_shell_syntax::available() {
            return;
        }
        assert_eq!(check_command("Get-ChildItem", None), None);
        assert_eq!(check_command("if then ((", Some("bash")), None);
    }
}
