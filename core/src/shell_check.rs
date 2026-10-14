//! Offline check that a `command` is written for the shell it runs under
//! (RAL-651).
//!
//! Uses the optional tree-sitter parse in `ralphus-shell-syntax`; without its
//! `parser` feature every check answers `None`. Only Bash-family and
//! PowerShell-family shells have a grammar, so a `cmd` (or unknown) shell is
//! never checked. Findings are advisory: the caller reports them as warnings.

use ralphus_shell_syntax::{Parsed, Shell, SpanKind, Status, parse};

use crate::shellcmd::shell_kind_from_exe;

fn grammar_for(shell: &str) -> Option<Shell> {
    match shell_kind_from_exe(shell)? {
        "bash" | "sh" | "zsh" => Some(Shell::Bash),
        "powershell" | "pwsh" => Some(Shell::PowerShell),
        _ => None,
    }
}

fn name(shell: Shell) -> &'static str {
    match shell {
        Shell::Bash => "bash",
        Shell::PowerShell => "powershell",
    }
}

/// `Verb-Noun` with both halves capitalized, the PowerShell cmdlet shape.
fn is_cmdlet_name(word: &str) -> bool {
    let Some((verb, noun)) = word.split_once('-') else {
        return false;
    };
    let capitalized = |s: &str| {
        s.chars().next().is_some_and(|c| c.is_ascii_uppercase())
            && s.chars().all(|c| c.is_ascii_alphanumeric())
    };
    capitalized(verb) && capitalized(noun)
}

/// Positive evidence that `text` (already parsed cleanly as PowerShell) was
/// written for PowerShell: a cmdlet invocation or an `$env:` variable.
fn looks_like_powershell(text: &str, parsed: &Parsed) -> bool {
    text.contains("$env:")
        || parsed
            .spans
            .iter()
            .filter(|s| s.kind == SpanKind::CommandName)
            .filter_map(|s| text.get(s.start..s.end))
            .any(is_cmdlet_name)
}

/// A warning message when `command` does not fit `shell` (the explicit
/// `shell` of the cell or proof step, `None` when unset), or `None` when it
/// fits, is ambiguous, or cannot be checked.
#[must_use]
pub fn check_command(command: &str, shell: Option<&str>) -> Option<String> {
    let Some(shell) = shell else {
        let parsed = parse(Shell::PowerShell, command);
        if parsed.status == Status::Ok && looks_like_powershell(command, &parsed) {
            return Some(
                "this command looks like PowerShell; want to set `shell = \"powershell\"` explicitly?"
                    .to_string(),
            );
        }
        return None;
    };
    let target = grammar_for(shell)?;
    let parsed = parse(target, command);
    if parsed.status != Status::Unparsed {
        return None;
    }
    let other = match target {
        Shell::Bash => Shell::PowerShell,
        Shell::PowerShell => Shell::Bash,
    };
    if parse(other, command).status == Status::Ok {
        return Some(format!(
            "this command looks like {other_name} but its shell is {shell}; fix the command or set `shell = \"{other_name}\"`",
            other_name = name(other),
        ));
    }
    let at = parsed
        .error_line
        .map_or(String::new(), |n| format!(" at line {n} of the command"));
    Some(format!(
        "syntax error{at}: not valid {} syntax",
        name(target)
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
    fn unset_shell_suggests_powershell_on_positive_evidence() {
        let msg = check_command("Get-ChildItem -Recurse", None).unwrap();
        assert!(msg.contains("shell = \"powershell\""), "{msg}");
        assert!(check_command("echo $env:HOME", None).is_some());
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
    fn cmd_shell_is_not_checked() {
        assert_eq!(check_command("Get-ChildItem", Some("cmd")), None);
    }
}

#[cfg(all(test, not(feature = "shell-syntax")))]
mod tests_off {
    use super::*;

    #[test]
    fn skipped_without_the_feature() {
        assert_eq!(check_command("Get-ChildItem", None), None);
        assert_eq!(check_command("if then ((", Some("bash")), None);
    }
}
