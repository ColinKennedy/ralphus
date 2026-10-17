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
/// validation: a command already in it is neither looked up nor reported
/// again. Dynamic names and paths are skipped.
#[must_use]
pub fn missing_command_warnings(
    command: &str,
    shell: Option<&str>,
    seen: &mut HashSet<String>,
) -> Vec<String> {
    missing_command_warnings_with(command, shell, seen, &command_exists)
}

type CommandResolver<'a> = dyn Fn(Shell, Option<&str>, &str) -> Option<bool> + 'a;

fn missing_command_warnings_with(
    command: &str,
    shell: Option<&str>,
    seen: &mut HashSet<String>,
    exists: &CommandResolver<'_>,
) -> Vec<String> {
    let Some((grammar, shell_name)) = selected_grammar(shell) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for exe in executable_names(command, shell) {
        if !seen.insert(format!("{shell_name}:{}", exe.to_lowercase())) {
            continue;
        }
        if !is_checkable(&exe) || exists(grammar, shell, &exe) != Some(false) {
            continue;
        }
        out.push(format!(
            "`{exe}` was not found by {shell_name} on this machine; check the selected shell or correct the command name (tools installed only in a later execution environment cannot be seen here)"
        ));
    }
    out
}

/// Dynamic names and paths need the execution environment to resolve them.
fn is_checkable(exe: &str) -> bool {
    if exe.contains([
        '/', '\\', '$', '%', '`', '*', '?', '(', ')', '{', '}', '"', '\'', '~', '=', '&', '|', '<',
        '>', '^', '!', ';', ' ', '\t', '\n',
    ]) {
        return false;
    }
    !exe.is_empty()
}

/// Ask the selected shell how it resolves the name. A failed shell launch is
/// indeterminate, so validation does not turn it into a missing-command warning.
fn command_exists(shell: Shell, selected_shell: Option<&str>, exe: &str) -> Option<bool> {
    use std::process::{Command, Stdio};

    let shell_program = selected_shell.unwrap_or(name(shell));
    if shell == Shell::Cmd {
        let where_status = Command::new("where.exe")
            .arg(exe)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .ok()?;
        if where_status.success() {
            return Some(true);
        }
        let help = Command::new(shell_program)
            .args(["/d", "/c", "help"])
            .output()
            .ok()?;
        if help.stdout.is_empty() {
            return None;
        }
        return Some(String::from_utf8_lossy(&help.stdout).lines().any(|line| {
            line.split_whitespace()
                .next()
                .is_some_and(|name| name.eq_ignore_ascii_case(exe))
        }));
    }
    let mut child = match shell {
        Shell::Bash => {
            let mut command = Command::new(shell_program);
            command.args(["-c", "command -v \"$1\" >/dev/null 2>&1", "_", exe]);
            command
        }
        Shell::PowerShell => {
            let mut command = Command::new(shell_program);
            command.args(["-NonInteractive", "-Command", "if (Get-Command -Name $env:RALPHUS_COMMAND_CHECK_NAME -ErrorAction SilentlyContinue) { exit 0 } else { exit 1 }"]);
            command.env("RALPHUS_COMMAND_CHECK_NAME", exe);
            command
        }
        Shell::Cmd => unreachable!(),
    };
    child.stdout(Stdio::null()).stderr(Stdio::null());
    child.status().ok().map(|status| status.success())
}

#[cfg(all(test, feature = "shell-syntax"))]
mod tests {
    use super::*;

    fn warnings(command: &str, shell: Option<&str>, present: &[&str]) -> Vec<String> {
        let mut seen = HashSet::new();
        missing_command_warnings_with(command, shell, &mut seen, &|_, _, e| {
            Some(present.contains(&e))
        })
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
        assert!(w[0].contains("correct the command name"), "{}", w[0]);
    }

    #[test]
    fn present_executable_is_silent() {
        assert!(warnings("cargo test", Some("bash"), &["cargo"]).is_empty());
    }

    #[test]
    fn repeated_executables_are_checked_once() {
        let calls = std::cell::Cell::new(0);
        let mut seen = HashSet::new();
        let exists = |_: Shell, _: Option<&str>, _: &str| {
            calls.set(calls.get() + 1);
            Some(false)
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
    fn shell_resolver_checks_builtins_and_aliases() {
        if cfg!(windows) {
            assert_eq!(command_exists(Shell::Cmd, None, "dir"), Some(true));
            assert_eq!(
                command_exists(Shell::Cmd, None, "nonsuchralphuscommand"),
                Some(false)
            );
            assert_eq!(
                command_exists(Shell::PowerShell, None, "Get-Alias"),
                Some(true)
            );
            assert_eq!(command_exists(Shell::PowerShell, None, "gci"), Some(true));
            assert_eq!(
                command_exists(
                    Shell::PowerShell,
                    None,
                    "Get-DefinitelyMissingRalphusCommand"
                ),
                Some(false)
            );
        } else {
            assert_eq!(command_exists(Shell::Bash, None, "cd"), Some(true));
            assert_eq!(
                command_exists(Shell::Bash, None, "definitely_missing_ralphus_command"),
                Some(false)
            );
        }
    }

    #[test]
    fn resolver_result_decides_warning_without_name_lists() {
        let mut seen = HashSet::new();
        let warnings = missing_command_warnings_with(
            "Get-Alias checkf; Get-FictionalThing",
            Some("powershell"),
            &mut seen,
            &|_, _, name| Some(name == "Get-Alias"),
        );
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].contains("Get-FictionalThing"));
    }

    #[test]
    fn resolver_receives_selected_shell() {
        let mut seen = HashSet::new();
        let warnings = missing_command_warnings_with(
            "checkf",
            Some("pwsh"),
            &mut seen,
            &|grammar, shell, name| {
                assert_eq!(grammar, Shell::PowerShell);
                assert_eq!(shell, Some("pwsh"));
                assert_eq!(name, "checkf");
                Some(true)
            },
        );
        assert!(warnings.is_empty());
    }

    #[test]
    fn unparsable_or_unknown_shell_is_not_checked() {
        assert!(warnings("if then ((", Some("bash"), &[]).is_empty());
        assert!(warnings("nosuchtool", Some("fish"), &[]).is_empty());
    }

    #[test]
    fn unavailable_resolver_does_not_warn() {
        let mut seen = HashSet::new();
        assert!(
            missing_command_warnings_with("unknown", Some("bash"), &mut seen, &|_, _, _| None)
                .is_empty()
        );
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
