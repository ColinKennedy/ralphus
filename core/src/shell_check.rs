//! Offline check that a `command` is written for the shell it runs under
//! (RAL-651).
//!
//! Which shell a text belongs to is decided entirely by the tree-sitter
//! grammars in `ralphus-shell-syntax`: the command is parsed with every
//! grammar and the one reporting the fewest syntax errors fits best. Nothing
//! here inspects the text itself. Without the crate's `parser` feature every
//! check answers `None`, and a shell with no grammar is never checked.
//! Findings are advisory: the caller reports them as warnings.

use ralphus_shell_syntax::{Parsed, Shell, Status, parse, shell_for_tool};

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

#[cfg(all(test, feature = "shell-syntax"))]
mod tests {
    use super::*;

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
